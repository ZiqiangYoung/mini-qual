use std::collections::HashSet;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::fs::File;
use std::fs::Metadata;
use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use serde::Serialize;

const NVME_READ: u8 = 0x02;
const NVME_FLUSH: u8 = 0x00;
const NAMESPACE_ID: u32 = 1;
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct Device {
    pub path: PathBuf,
    pub serial: String,
    pub logical_block_bytes: u64,
    pub size_bytes: u64,
    pub discard_max_bytes: u64,
    controller: PathBuf,
    namespace: PathBuf,
    disk_sequence: Option<u64>,
    device_id: DeviceId,
}

pub struct DeviceLease {
    _lock: File,
    pub partially_inspected_pids: Vec<u32>,
}

impl Device {
    pub fn discover(serial: &str) -> Result<Self> {
        let controller = find_controller(Path::new("/sys/class/nvme"), serial)?;
        let namespace = find_namespace(&controller, Path::new("/sys/class/nvme-subsystem"))?;
        let name = namespace.file_name().context("namespace has no name")?;
        let path = Path::new("/dev").join(name);
        let device_id = read_device_id(&namespace, DeviceKind::Block)?;
        let logical_block_bytes = read_number(&namespace.join("queue/logical_block_size"))?;
        ensure!(
            logical_block_bytes >= 512 && logical_block_bytes.is_power_of_two(),
            "invalid logical block size {logical_block_bytes} for {}",
            path.display()
        );
        let size_bytes = volume_bytes(&namespace, logical_block_bytes)?;
        let discard_max_bytes = read_number(&namespace.join("queue/discard_max_bytes"))?;
        let disk_sequence = optional_number(&namespace.join("diskseq"))?;
        let device = Self {
            path,
            serial: serial.to_owned(),
            logical_block_bytes,
            size_bytes,
            discard_max_bytes,
            controller,
            namespace,
            disk_sequence,
            device_id,
        };
        device.check_ready()?;
        Ok(device)
    }

    /// Holds an advisory exclusive lock on the block node for the life of the returned file.
    pub fn acquire(&self) -> Result<DeviceLease> {
        self.check_ready()?;
        let file = self.open(true, 0)?;
        // SAFETY: The live File owns this descriptor. flock does not take a pointer.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error()).with_context(|| {
                format!("could not exclusively lock device {}", self.path.display())
            });
        }
        let targets = self.targets()?;
        let partially_inspected_pids = check_processes(Path::new("/proc"), &targets)?;
        self.check_ready()?;
        // A whole-device exclusive open detects kernel claims on it or its partitions.
        // Close the probe before fio's nonexclusive descriptor issues BLKDISCARD.
        self.open(true, libc::O_EXCL).with_context(|| {
            format!(
                "checking for a mount, swap, or other exclusive use of {}",
                self.path.display()
            )
        })?;
        Ok(DeviceLease {
            _lock: file,
            partially_inspected_pids,
        })
    }

    /// Checks identity without waiting for a resetting controller.
    pub fn check_present(&self) -> Result<()> {
        ensure!(
            read_string(&self.controller.join("serial"))? == self.serial,
            "controller serial changed for {}",
            self.path.display()
        );
        check_device_node(&self.path, self.device_id)?;
        ensure!(
            optional_number(&self.namespace.join("diskseq"))? == self.disk_sequence,
            "block device was replaced at {}",
            self.path.display()
        );
        Ok(())
    }

    pub fn check_ready(&self) -> Result<()> {
        let start = Instant::now();
        loop {
            let state = read_string(&self.controller.join("state"))?;
            if state == "live" {
                break;
            }
            ensure!(
                matches!(state.as_str(), "new" | "resetting" | "connecting")
                    && start.elapsed() < RECOVERY_TIMEOUT,
                "controller for {} is {state}",
                self.path.display()
            );
            thread::sleep(Duration::from_millis(250));
        }

        self.check_present()?;
        ensure!(
            read_number(&self.namespace.join("nsid"))? == u64::from(NAMESPACE_ID),
            "namespace ID changed for {}",
            self.path.display()
        );
        ensure!(
            read_number(&self.namespace.join("queue/logical_block_size"))?
                == self.logical_block_bytes
                && volume_bytes(&self.namespace, self.logical_block_bytes)? == self.size_bytes,
            "namespace geometry changed for {}",
            self.path.display()
        );
        ensure!(
            read_number(&self.namespace.join("ro"))? == 0,
            "device {} is read-only",
            self.path.display()
        );
        Ok(())
    }

    pub fn flush(&self) -> Result<()> {
        self.check_ready()?;
        let file = self.open(true, 0)?;
        let mut command = NvmeCommand {
            opcode: NVME_FLUSH,
            nsid: NAMESPACE_ID,
            ..NvmeCommand::default()
        };
        match submit(&file, &mut command) {
            Ok(0) => Ok(()),
            Ok(status) => bail!(
                "NVMe Flush failed on {} with status {status:#x}, result {:#x}",
                self.path.display(),
                command.result
            ),
            // The Linux block layer issues a flush for a cache it reports as write-back.
            Err(error) if ioctl_unavailable(&error) => file
                .sync_all()
                .with_context(|| format!("block-device flush failed on {}", self.path.display())),
            Err(error) => Err(error)
                .with_context(|| format!("NVMe Flush ioctl failed on {}", self.path.display())),
        }
    }

    pub fn open_reader(&self) -> Result<BlockReader> {
        self.check_ready()?;
        let file = self.open(false, libc::O_DIRECT)?;
        let block_bytes = usize::try_from(self.logical_block_bytes)?;
        // SAFETY: sysconf only reads process configuration for this selector.
        let page_bytes = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page_bytes = usize::try_from(page_bytes).context("could not determine page size")?;
        let dma_mask = optional_number(&self.namespace.join("queue/dma_alignment"))?.unwrap_or(0);
        let dma_bytes = usize::try_from(dma_mask)?.saturating_add(1);
        let alignment = block_bytes.max(page_bytes).max(dma_bytes);
        ensure!(alignment.is_power_of_two(), "invalid direct I/O alignment");
        let storage = vec![0; block_bytes + alignment - 1];
        let buffer_offset = storage.as_ptr().align_offset(alignment);
        let metadata_bytes = optional_number(&self.namespace.join("metadata_bytes"))?.unwrap_or(0);
        Ok(BlockReader {
            file,
            storage,
            buffer_offset,
            block_bytes,
            block_count: self.size_bytes / self.logical_block_bytes,
            // Read formatted namespaces with metadata through the kernel's block driver.
            use_passthrough: metadata_bytes == 0,
        })
    }

    fn open(&self, write: bool, flags: libc::c_int) -> Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(write)
            .custom_flags(flags | libc::O_NOFOLLOW)
            .open(&self.path)
            .with_context(|| format!("opening block device {}", self.path.display()))?;
        ensure!(
            self.device_id.matches(&file.metadata()?),
            "block device changed while opening {}",
            self.path.display()
        );
        Ok(file)
    }

    fn targets(&self) -> Result<Targets> {
        let mut targets = Targets::default();
        targets.insert(&self.controller, DeviceKind::Character)?;
        targets.insert(&self.namespace, DeviceKind::Block)?;
        for namespace in namespace_list(&self.controller)? {
            targets.insert(&namespace, DeviceKind::Block)?;
        }
        if let Some(suffix) = self
            .namespace
            .file_name()
            .and_then(OsStr::to_str)
            .and_then(|name| name.strip_prefix("nvme"))
        {
            let generic = self.namespace.with_file_name(format!("ng{suffix}"));
            if generic.join("dev").is_file() {
                targets.insert(&generic, DeviceKind::Character)?;
            }
        }
        for entry in fs::read_dir(&self.namespace)? {
            let path = entry?.path();
            if path.join("partition").is_file() {
                targets.insert(&path, DeviceKind::Block)?;
            }
        }
        Ok(targets)
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReadStatus {
    Success,
    Nvme { status: i32, result: u32 },
    Os { errno: Option<i32>, message: String },
    Short { bytes: usize },
}

pub struct BlockReader {
    file: File,
    storage: Vec<u8>,
    buffer_offset: usize,
    block_bytes: usize,
    block_count: u64,
    use_passthrough: bool,
}

impl BlockReader {
    pub fn read_block(&mut self, lba: u64) -> Result<ReadStatus> {
        ensure!(lba < self.block_count, "LBA {lba} outside namespace");
        let buffer = &mut self.storage[self.buffer_offset..self.buffer_offset + self.block_bytes];
        if self.use_passthrough {
            let mut command = NvmeCommand {
                opcode: NVME_READ,
                nsid: NAMESPACE_ID,
                address: buffer.as_mut_ptr() as u64,
                data_len: u32::try_from(buffer.len())?,
                cdw10: lba as u32,
                cdw11: (lba >> 32) as u32,
                // cdw12's low 16 bits are zero for a single logical block.
                ..NvmeCommand::default()
            };
            match submit(&self.file, &mut command) {
                Ok(0) => return Ok(ReadStatus::Success),
                Ok(status) => {
                    return Ok(ReadStatus::Nvme {
                        status,
                        result: command.result,
                    });
                }
                Err(error) if ioctl_unavailable(&error) => self.use_passthrough = false,
                Err(error) => return Ok(os_status(error)),
            }
        }
        Ok(
            match self.file.read_at(buffer, lba * self.block_bytes as u64) {
                Ok(bytes) if bytes == self.block_bytes => ReadStatus::Success,
                Ok(bytes) => ReadStatus::Short { bytes },
                Err(error) => os_status(error),
            },
        )
    }
}

fn os_status(error: io::Error) -> ReadStatus {
    ReadStatus::Os {
        errno: error.raw_os_error(),
        message: error.to_string(),
    }
}

// Linux UAPI struct nvme_passthru_cmd, from <linux/nvme_ioctl.h>.
#[repr(C)]
#[derive(Default)]
struct NvmeCommand {
    opcode: u8,
    flags: u8,
    reserved: u16,
    nsid: u32,
    cdw2: u32,
    cdw3: u32,
    metadata: u64,
    address: u64,
    metadata_len: u32,
    data_len: u32,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
    timeout_ms: u32,
    result: u32,
}

fn submit(file: &File, cmd: &mut NvmeCommand) -> io::Result<i32> {
    let request = libc::_IOWR::<NvmeCommand>(u32::from(b'N'), 0x43);
    // SAFETY: cmd has the Linux NVME_IOCTL_IO_CMD layout and is live and mutable for the
    // synchronous call. When data_len is nonzero, address is a live aligned output buffer
    // of that size which the calling read exclusively borrows until the ioctl returns.
    let status = unsafe { libc::ioctl(file.as_raw_fd(), request, cmd) };
    if status < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(status)
    }
}

fn ioctl_unavailable(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EACCES | libc::EPERM | libc::ENOTTY | libc::EOPNOTSUPP | libc::ENOSYS)
    )
}

fn find_controller(class: &Path, serial: &str) -> Result<PathBuf> {
    let mut matched = None;
    for entry in fs::read_dir(class).with_context(|| format!("listing {}", class.display()))? {
        let entry = entry?;
        let serial_path = entry.path().join("serial");
        let value = match fs::read_to_string(&serial_path) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", serial_path.display()));
            }
        };
        if value.trim() == serial {
            ensure!(
                matched.is_none(),
                "multiple NVMe controllers have serial {serial}"
            );
            matched = Some(entry.path());
        }
    }
    matched.with_context(|| format!("no NVMe controller found with serial {serial}"))
}

fn find_namespace(controller: &Path, subsystems: &Path) -> Result<PathBuf> {
    let mut namespaces = namespace_list(controller)?;
    if namespaces.len() == 1 && optional_number(&namespaces[0].join("hidden"))? == Some(1) {
        // Linux native multipathing places the usable block node under the namespace head.
        let controller_name = controller.file_name().context("controller has no name")?;
        for entry in fs::read_dir(subsystems)? {
            let subsystem = entry?.path();
            if subsystem.join(controller_name).is_dir() {
                namespaces = namespace_list(&subsystem)?;
                break;
            }
        }
    }
    ensure!(
        namespaces.len() == 1
            && read_number(&namespaces[0].join("nsid"))? == u64::from(NAMESPACE_ID)
            && optional_number(&namespaces[0].join("hidden"))? != Some(1),
        "{} must expose exactly one block namespace with NSID 1 (found {})",
        controller.display(),
        namespaces.len()
    );
    Ok(namespaces.remove(0))
}

fn namespace_list(parent: &Path) -> Result<Vec<PathBuf>> {
    let mut namespaces = Vec::new();
    for entry in fs::read_dir(parent).with_context(|| format!("listing {}", parent.display()))? {
        let path = entry?.path();
        if path.join("nsid").is_file() && path.join("dev").is_file() {
            namespaces.push(path);
        }
    }
    Ok(namespaces)
}

fn read_string(path: &Path) -> Result<String> {
    Ok(fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?
        .trim()
        .to_owned())
}

fn read_number(path: &Path) -> Result<u64> {
    read_string(path)?
        .parse()
        .with_context(|| format!("parsing number in {}", path.display()))
}

fn optional_number(path: &Path) -> Result<Option<u64>> {
    match fs::read_to_string(path) {
        Ok(value) => {
            Ok(Some(value.trim().parse().with_context(|| {
                format!("parsing number in {}", path.display())
            })?))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn volume_bytes(namespace: &Path, logical_block_bytes: u64) -> Result<u64> {
    // Linux sysfs expresses the size of a block device in 512-byte sectors.
    let size = read_number(&namespace.join("size"))?
        .checked_mul(512)
        .context("namespace capacity exceeds u64")?;
    ensure!(
        size > 0 && size % logical_block_bytes == 0,
        "namespace capacity {size} is not a positive multiple of {logical_block_bytes}"
    );
    Ok(size)
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum DeviceKind {
    Block,
    Character,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct DeviceId {
    kind: DeviceKind,
    number: u64,
}

impl DeviceId {
    fn from_metadata(metadata: &Metadata) -> Option<Self> {
        let file_type = metadata.file_type();
        let kind = if file_type.is_block_device() {
            DeviceKind::Block
        } else if file_type.is_char_device() {
            DeviceKind::Character
        } else {
            return None;
        };
        Some(Self {
            kind,
            number: metadata.rdev(),
        })
    }

    fn matches(self, metadata: &Metadata) -> bool {
        Self::from_metadata(metadata) == Some(self)
    }
}

fn read_device_id(path: &Path, kind: DeviceKind) -> Result<DeviceId> {
    let dev_file = path.join("dev");
    let number = read_string(&dev_file)?;
    let (major, minor) = number
        .split_once(':')
        .with_context(|| format!("invalid major:minor in {}", dev_file.display()))?;
    Ok(DeviceId {
        kind,
        number: libc::makedev(major.parse()?, minor.parse()?),
    })
}

fn check_device_node(path: &Path, id: DeviceId) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("checking block device node {}", path.display()))?;
    ensure!(
        id.matches(&metadata),
        "device node identity changed at {}",
        path.display()
    );
    Ok(())
}

#[derive(Default)]
struct Targets {
    ids: HashSet<DeviceId>,
    names: HashSet<OsString>,
}

impl Targets {
    fn insert(&mut self, sysfs_node: &Path, kind: DeviceKind) -> Result<()> {
        self.ids.insert(read_device_id(sysfs_node, kind)?);
        self.names.insert(
            sysfs_node
                .file_name()
                .context("device has no name")?
                .to_owned(),
        );
        Ok(())
    }

    fn contains(&self, metadata: &Metadata) -> bool {
        DeviceId::from_metadata(metadata).is_some_and(|id| self.ids.contains(&id))
    }

    fn matches_argument(&self, process: &Path, argument: &[u8]) -> bool {
        let mut values = argument.splitn(2, |byte| *byte == b'=');
        values.any(|value| {
            self.matches_path(process, Path::new(OsStr::from_bytes(value)))
                || (value.contains(&b':')
                    && value
                        .split(|byte| *byte == b':')
                        .any(|item| self.matches_path(process, Path::new(OsStr::from_bytes(item)))))
        })
    }

    fn matches_path(&self, process: &Path, path: &Path) -> bool {
        if self.names.contains(path.as_os_str()) {
            return true;
        }
        if let Ok(name) = path.strip_prefix("/dev") {
            if self.names.contains(name.as_os_str()) {
                return true;
            }
        }
        if path.as_os_str().is_empty() || path.as_os_str().as_bytes().starts_with(b"-") {
            return false;
        }
        let resolved = if let Ok(path) = path.strip_prefix("/") {
            process.join("root").join(path)
        } else {
            process.join("cwd").join(path)
        };
        fs::metadata(resolved).is_ok_and(|metadata| self.contains(&metadata))
    }
}

#[cfg(test)]
#[path = "../tests/device/mod.rs"]
mod tests;

fn proc_entry_disappeared(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

fn check_processes(proc_root: &Path, targets: &Targets) -> Result<Vec<u32>> {
    // SAFETY: getuid has no arguments and returns the current process's real Linux UID.
    let uid = unsafe { libc::getuid() };
    let mut partially_inspected_pids = Vec::new();
    for entry in fs::read_dir(proc_root)? {
        let entry = entry?;
        let pid = match entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        {
            Some(pid) if pid != std::process::id() => pid,
            _ => continue,
        };

        let process = entry.path();
        let status = match fs::read_to_string(process.join("status")) {
            Ok(status) => status,
            Err(error) if proc_entry_disappeared(&error) => continue,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                // A procfs mount with hidepid may expose a directory without its status.
                match entry.metadata() {
                    Ok(metadata) if metadata.uid() != uid => continue,
                    Err(error) if proc_entry_disappeared(&error) => continue,
                    _ => return Err(error).with_context(|| format!("reading UID of PID {pid}")),
                }
            }
            Err(error) => return Err(error).with_context(|| format!("reading UID of PID {pid}")),
        };
        let owner_uid = status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .and_then(|line| line.split_ascii_whitespace().next())
            .and_then(|value| value.parse::<u32>().ok())
            .with_context(|| format!("could not determine UID of PID {pid}"))?;
        if owner_uid != uid {
            continue;
        }
        if !inspect_process(&process, pid, targets)? {
            partially_inspected_pids.push(pid);
        }
    }
    partially_inspected_pids.sort_unstable();
    Ok(partially_inspected_pids)
}

fn inspect_process(process: &Path, pid: u32, targets: &Targets) -> Result<bool> {
    let mut complete = true;
    let descriptors = match fs::read_dir(process.join("fd")) {
        Ok(descriptors) => Some(descriptors),
        Err(error) if proc_entry_disappeared(&error) => None,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            complete = false;
            None
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting open descriptors of PID {pid}"));
        }
    };
    for descriptor in descriptors.into_iter().flatten() {
        let descriptor = descriptor?;
        match fs::metadata(descriptor.path()) {
            Ok(metadata) => ensure!(
                !targets.contains(&metadata),
                "PID {pid} has a target device handle open (FD {})",
                descriptor.file_name().to_string_lossy()
            ),
            Err(error) if proc_entry_disappeared(&error) => continue,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => complete = false,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("checking descriptor {}", descriptor.path().display())
                });
            }
        }
    }

    let command_line = match fs::read(process.join("cmdline")) {
        Ok(command_line) => command_line,
        Err(error) if proc_entry_disappeared(&error) => return Ok(complete),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("reading command line of PID {pid}"));
        }
    };
    ensure!(
        !command_line
            .split(|byte| *byte == 0)
            .any(|argument| targets.matches_argument(process, argument)),
        "PID {pid} references the target device in its command line"
    );
    Ok(complete)
}
