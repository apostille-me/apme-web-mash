use std::{
    fs,
    io,
    os::unix::{
        fs::{symlink, FileTypeExt, PermissionsExt},
        net::UnixListener,
    },
    path::{Path, PathBuf},
};

pub(super) fn parse_listener_uri(value: &str) -> io::Result<PathBuf> {
    let trimmed = value.trim();
    let raw = trimmed
        .strip_prefix("unix://")
        .or_else(|| trimmed.strip_prefix("unix:"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "FN_LISTENER must use a unix: or unix:// URI",
            )
        })?;
    let path = Path::new(raw);
    if raw.trim().is_empty() || !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "FN_LISTENER must name an absolute Unix socket path",
        ));
    }
    Ok(path.to_path_buf())
}

pub(super) fn bind(socket_path: &Path) -> io::Result<UnixListener> {
    let parent = socket_path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "FN_LISTENER socket path must have a parent directory",
        )
    })?;
    let file_name = socket_path
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "FN_LISTENER socket path must have a valid file name",
            )
        })?;
    let bound_path = parent.join(format!("phony{file_name}"));

    remove_previous_socket(socket_path)?;
    remove_previous_socket(&bound_path)?;

    let listener = UnixListener::bind(&bound_path)?;
    fs::set_permissions(&bound_path, fs::Permissions::from_mode(0o666))?;
    let bound_name = bound_path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "invalid bound socket name")
    })?;
    symlink(bound_name, socket_path)?;
    Ok(listener)
}

fn remove_previous_socket(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if !file_type.is_socket() && !file_type.is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "refusing to replace a non-socket FN_LISTENER path: {}",
                        path.display()
                    ),
                ));
            }
            fs::remove_file(path)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn parses_fn_listener_uris() {
        assert_eq!(
            parse_listener_uri("unix:///tmp/fn.sock").unwrap(),
            PathBuf::from("/tmp/fn.sock"),
        );
        assert_eq!(
            parse_listener_uri("unix:/tmp/fn.sock").unwrap(),
            PathBuf::from("/tmp/fn.sock"),
        );
        assert!(parse_listener_uri("tcp://127.0.0.1:8080").is_err());
        assert!(parse_listener_uri("unix:relative.sock").is_err());
    }

    #[test]
    fn binds_the_fn_compatible_phony_socket_and_public_symlink() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "scintilla-fn-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let public_path = root.join("lsnr.sock");
        let phony_path = root.join("phonylsnr.sock");

        let listener = bind(&public_path).unwrap();
        assert!(fs::symlink_metadata(&public_path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(fs::symlink_metadata(&phony_path)
            .unwrap()
            .file_type()
            .is_socket());

        drop(listener);
        fs::remove_file(public_path).unwrap();
        fs::remove_file(phony_path).unwrap();
        fs::remove_dir(root).unwrap();
    }
}
