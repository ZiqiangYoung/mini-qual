use std::fs;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use uuid::Uuid;

use super::DeviceId;
use super::Targets;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("mini-qual-device-{}", Uuid::new_v4()));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }

    fn write(&self, name: &str, bytes: impl AsRef<[u8]>) -> Result<()> {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().expect("fixture path has a parent"))?;
        fs::write(path, bytes)?;
        Ok(())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn selects_only_an_exact_unique_serial() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.write("nvme0/serial", "001-A  \n")?;
    fixture.write("nvme1/serial", "001-B\n")?;
    assert_eq!(
        super::find_controller(&fixture.0, "001-A")?,
        fixture.0.join("nvme0")
    );
    assert!(super::find_controller(&fixture.0, "001").is_err());
    fixture.write("nvme1/serial", "001-A\n")?;
    assert!(super::find_controller(&fixture.0, "001-A").is_err());
    Ok(())
}

#[test]
fn requires_one_namespace_with_id_one_and_aligned_visible_capacity() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.write("nvme5/nvme5n1/nsid", "1")?;
    fixture.write("nvme5/nvme5n1/dev", "259:0")?;
    fixture.write("nvme5/nvme5n1/size", "32")?;
    fixture.write("nvme5/nvme5n1/nvme5n1p1/dev", "259:1")?;
    fixture.write("nvme5/ng5n1/dev", "245:0")?;
    let controller = fixture.0.join("nvme5");
    let namespace = super::find_namespace(&controller, &fixture.0)?;
    assert_eq!(namespace, controller.join("nvme5n1"));
    assert_eq!(super::volume_bytes(&namespace, 4096)?, 16_384);

    fixture.write("nvme5/nvme5n1/size", "31")?;
    assert!(super::volume_bytes(&namespace, 4096).is_err());
    fixture.write("nvme5/nvme5n1/nsid", "2")?;
    assert!(super::find_namespace(&controller, &fixture.0).is_err());
    fixture.write("nvme5/nvme5n1/nsid", "1")?;
    fixture.write("nvme5/nvme5n2/nsid", "2")?;
    fixture.write("nvme5/nvme5n2/dev", "259:2")?;
    assert!(super::find_namespace(&controller, &fixture.0).is_err());
    Ok(())
}

#[test]
fn resolves_hidden_path_to_its_block_namespace_head() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.write("controllers/nvme3/nvme7c3n1/nsid", "1")?;
    fixture.write("controllers/nvme3/nvme7c3n1/dev", "259:5")?;
    fixture.write("controllers/nvme3/nvme7c3n1/hidden", "1")?;
    fixture.write("subsystems/nvme-subsys7/nvme7n1/nsid", "1")?;
    fixture.write("subsystems/nvme-subsys7/nvme7n1/dev", "259:6")?;
    let controller = fixture.0.join("controllers/nvme3");
    let subsystems = fixture.0.join("subsystems");
    symlink(&controller, subsystems.join("nvme-subsys7/nvme3"))?;
    assert_eq!(
        super::find_namespace(&controller, &subsystems)?,
        subsystems.join("nvme-subsys7/nvme7n1")
    );
    Ok(())
}

#[test]
fn checks_descriptors_and_args_for_the_current_uid() -> Result<()> {
    let fixture = Fixture::new()?;
    let proc_root = fixture.0.join("proc");
    // SAFETY: getuid has no arguments and returns the current process's real Linux UID.
    let uid = unsafe { libc::getuid() };
    let foreign = uid.wrapping_add(1);
    fixture.write(
        "proc/4294967290/status",
        format!("Uid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
    )?;
    fixture.write(
        "proc/4294967291/status",
        format!("Uid:\t{foreign}\t{foreign}\t{foreign}\t{foreign}\n"),
    )?;
    fixture.write("proc/4294967290/cmdline", b"fio\0--filename=/dev/nvme2n1\0")?;
    fixture.write("proc/4294967291/cmdline", b"fio\0--filename=/dev/nvme1n1\0")?;
    let owned = proc_root.join("4294967290");
    let foreign = proc_root.join("4294967291");
    fs::create_dir(owned.join("fd"))?;
    fs::create_dir(foreign.join("fd"))?;
    symlink("/dev/null", foreign.join("fd/1"))?;
    let target =
        DeviceId::from_metadata(&fs::metadata("/dev/null")?).expect("null is a character device");
    let mut targets = Targets::default();
    targets.ids.insert(target);
    targets.names.insert("nvme1n1".into());
    super::check_processes(&proc_root, &targets)?;

    symlink("/dev/null", owned.join("fd/1"))?;
    let error = super::check_processes(&proc_root, &targets)
        .expect_err("target descriptor must be detected");
    assert!(error.to_string().contains("4294967290") && error.to_string().contains("FD 1"));
    fs::remove_file(owned.join("fd/1"))?;

    fixture.write(
        "proc/4294967290/cmdline",
        b"fio\0--filename=/dev/nvme2n1:/dev/nvme1n1\0",
    )?;
    let error =
        super::check_processes(&proc_root, &targets).expect_err("target argument must be detected");
    assert!(error.to_string().contains("command line"));

    symlink(Path::new("/dev"), owned.join("cwd"))?;
    fixture.write("proc/4294967290/cmdline", b"fio\0--filename=null\0")?;
    let error =
        super::check_processes(&proc_root, &targets).expect_err("target alias must be detected");
    assert!(error.to_string().contains("command line"));
    Ok(())
}

#[test]
fn reports_restricted_descriptors_and_still_checks_command_line() -> Result<()> {
    // SAFETY: getuid has no arguments and returns the current process's real Linux UID.
    let uid = unsafe { libc::getuid() };
    if uid == 0 {
        // The directory permissions below do not restrict a typical privileged test runner.
        return Ok(());
    }
    let fixture = Fixture::new()?;
    let proc_root = fixture.0.join("proc");
    let fd = proc_root.join("4294967290/fd");
    fixture.write("proc/4294967290/status", format!("Uid:\t{uid}\n"))?;
    fixture.write("proc/4294967290/cmdline", b"fio\0--filename=/dev/nvme2n1\0")?;
    fs::create_dir(&fd)?;
    fs::set_permissions(&fd, Permissions::from_mode(0o000))?;
    let mut targets = Targets::default();
    targets.names.insert("nvme1n1".into());
    let restricted = super::check_processes(&proc_root, &targets);
    fixture.write("proc/4294967290/cmdline", b"fio\0--filename=/dev/nvme1n1\0")?;
    let occupied = super::check_processes(&proc_root, &targets);
    fs::set_permissions(&fd, Permissions::from_mode(0o700))?;
    assert_eq!(restricted?, vec![4294967290]);
    assert!(
        occupied
            .expect_err("target argument must be detected")
            .to_string()
            .contains("command line")
    );
    Ok(())
}
