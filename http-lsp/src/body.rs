//! Request body assembly (inline text, `<` / `<@` file includes, multipart) and `>>` / `>>!`
//! response redirects. Relative paths resolve against the `.http` file's directory.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use tokio::io::AsyncReadExt;

use crate::{
    protocol::{PreparedBody, MAX_BODY_BYTES},
    syntax::{Body, BodyPart, MultipartPart},
};

/// Replaces `{{ }}` variables in text.
pub type Substitute<'a> = dyn FnMut(&str) -> String + Send + 'a;

/// Builds the request body. For multipart bodies, `boundary` is the boundary already resolved
/// for the `Content-Type` header, so a dynamic boundary such as `{{$uuid}}` matches it.
pub async fn prepare(
    body: &Body,
    boundary: Option<&str>,
    base_dir: &Path,
    substitute: &mut Substitute<'_>,
) -> Result<PreparedBody, String> {
    let bytes = match body {
        Body::Parts(parts) if parts.is_empty() => return Ok(PreparedBody::Empty),
        Body::Parts(parts) => join_parts(parts, base_dir, substitute).await?,
        Body::Multipart {
            boundary: raw,
            parts,
        } => {
            let boundary = match boundary {
                Some(boundary) => boundary.to_owned(),
                None => substitute(raw),
            };
            multipart(&boundary, parts, base_dir, substitute).await?
        }
    };
    if bytes.len() > MAX_BODY_BYTES {
        return Err(format!(
            "request body exceeds the {} MiB limit",
            MAX_BODY_BYTES / 1024 / 1024
        ));
    }
    Ok(PreparedBody::Bytes(bytes))
}

/// Parts are separated by newlines, as they were in the source file.
async fn join_parts(
    parts: &[BodyPart],
    base_dir: &Path,
    substitute: &mut Substitute<'_>,
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            bytes.push(b'\n');
        }
        match part {
            BodyPart::Text(text) => bytes.extend_from_slice(substitute(text).as_bytes()),
            BodyPart::File {
                path,
                substitute: false,
            } => {
                bytes.extend_from_slice(&read_include(base_dir, &substitute(path)).await?);
            }
            BodyPart::File {
                path,
                substitute: true,
            } => {
                let contents = read_include(base_dir, &substitute(path)).await?;
                let text = String::from_utf8(contents)
                    .map_err(|_| format!("{path} is not UTF-8 text, so it cannot use <@"))?;
                bytes.extend_from_slice(substitute(&text).as_bytes());
            }
        }
        if bytes.len() > MAX_BODY_BYTES {
            break;
        }
    }
    Ok(bytes)
}

async fn multipart(
    boundary: &str,
    parts: &[MultipartPart],
    base_dir: &Path,
    substitute: &mut Substitute<'_>,
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    for part in parts {
        bytes.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        for header in &part.headers {
            let line = format!(
                "{}: {}\r\n",
                substitute(&header.name),
                substitute(&header.value)
            );
            bytes.extend_from_slice(line.as_bytes());
        }
        bytes.extend_from_slice(b"\r\n");
        bytes.extend_from_slice(&join_parts(&part.body, base_dir, substitute).await?);
        bytes.extend_from_slice(b"\r\n");
        if bytes.len() > MAX_BODY_BYTES {
            return Ok(bytes);
        }
    }
    bytes.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Ok(bytes)
}

/// Reads a regular file of at most [`MAX_BODY_BYTES`]. Devices and FIFOs are rejected before
/// opening, and the read itself is bounded in case the file grows.
async fn read_include(base_dir: &Path, path: &str) -> Result<Vec<u8>, String> {
    let path = base_dir.join(path);
    let failed = |error: io::Error| format!("failed to read {}: {error}", path.display());
    let too_large = || {
        format!(
            "{} is larger than the {} MiB body limit",
            path.display(),
            MAX_BODY_BYTES / 1024 / 1024
        )
    };
    let metadata = tokio::fs::metadata(&path).await.map_err(failed)?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if metadata.len() > MAX_BODY_BYTES as u64 {
        return Err(too_large());
    }
    let file = tokio::fs::File::open(&path).await.map_err(failed)?;
    let mut contents = Vec::new();
    file.take(MAX_BODY_BYTES as u64 + 1)
        .read_to_end(&mut contents)
        .await
        .map_err(failed)?;
    if contents.len() > MAX_BODY_BYTES {
        return Err(too_large());
    }
    Ok(contents)
}

/// Writes a response body for `>>` (a fresh `name-N.ext` when the file exists) or `>>!`
/// (overwrite). Returns the path written.
pub fn write_redirect(
    base_dir: &Path,
    path: &str,
    overwrite: bool,
    body: &[u8],
) -> Result<PathBuf, String> {
    let target = base_dir.join(path);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }
    if overwrite {
        fs::write(&target, body)
            .map_err(|error| format!("failed to write {}: {error}", target.display()))?;
        return Ok(target);
    }
    for attempt in 0..10_000 {
        let candidate = if attempt == 0 {
            target.clone()
        } else {
            numbered(&target, attempt)
        };
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                io::Write::write_all(&mut file, body)
                    .map_err(|error| format!("failed to write {}: {error}", candidate.display()))?;
                return Ok(candidate);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("failed to write {}: {error}", candidate.display())),
        }
    }
    Err(format!("no free file name next to {}", target.display()))
}

fn numbered(path: &Path, number: u32) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match path.extension() {
        Some(extension) => format!("{stem}-{number}.{}", extension.to_string_lossy()),
        None => format!("{stem}-{number}"),
    };
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use std::{
        env, process,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::syntax;

    fn temporary_directory() -> PathBuf {
        let directory = env::temp_dir().join(format!(
            "zed-http-body-{}-{}",
            process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn upper(text: &str) -> String {
        text.replace("{{name}}", "World")
    }

    #[tokio::test]
    async fn joins_text_and_file_includes() {
        let directory = temporary_directory();
        fs::write(directory.join("raw.txt"), "raw {{name}}").unwrap();
        fs::write(directory.join("template.txt"), "hello {{name}}").unwrap();
        let document =
            syntax::parse("POST http://x\n\nstart {{name}}\n< ./raw.txt\n<@ ./template.txt\n");
        let body = prepare(&document.blocks[0].body, None, &directory, &mut upper)
            .await
            .unwrap();
        assert_eq!(
            body,
            PreparedBody::Bytes(b"start World\nraw {{name}}\nhello World".to_vec())
        );

        let missing = syntax::parse("POST http://x\n\n< ./missing.txt\n");
        let error = prepare(&missing.blocks[0].body, None, &directory, &mut upper)
            .await
            .unwrap_err();
        assert!(error.contains("missing.txt"), "{error}");
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_devices_and_directories() {
        for path in ["/dev/zero", "/tmp"] {
            let document = syntax::parse(&format!("POST http://x\n\n< {path}\n"));
            let error = prepare(&document.blocks[0].body, None, Path::new("/"), &mut upper)
                .await
                .unwrap_err();
            assert!(error.contains("not a regular file"), "{error}");
        }
    }

    #[tokio::test]
    async fn builds_multipart_bodies() {
        let directory = temporary_directory();
        fs::write(directory.join("upload.txt"), "file contents").unwrap();
        let document = syntax::parse(
            "POST http://x\nContent-Type: multipart/form-data; boundary=B\n\n--B\n\
             Content-Disposition: form-data; name=\"field\"\n\n{{name}}\n--B\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"upload.txt\"\n\n\
             < ./upload.txt\n--B--\n",
        );
        let PreparedBody::Bytes(body) =
            prepare(&document.blocks[0].body, None, &directory, &mut upper)
                .await
                .unwrap()
        else {
            panic!("expected a body");
        };
        assert_eq!(
            String::from_utf8(body).unwrap(),
            "--B\r\nContent-Disposition: form-data; name=\"field\"\r\n\r\nWorld\r\n\
             --B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"upload.txt\"\r\n\r\n\
             file contents\r\n--B--\r\n"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn redirects_pick_unique_names_unless_overwriting() {
        let directory = temporary_directory();
        let first = write_redirect(&directory, "out/body.json", false, b"1").unwrap();
        let second = write_redirect(&directory, "out/body.json", false, b"2").unwrap();
        assert_eq!(first, directory.join("out/body.json"));
        assert_eq!(second, directory.join("out/body-1.json"));
        let replaced = write_redirect(&directory, "out/body.json", true, b"3").unwrap();
        assert_eq!(replaced, first);
        assert_eq!(fs::read(&first).unwrap(), b"3");
        assert_eq!(fs::read(&second).unwrap(), b"2");
        fs::remove_dir_all(directory).unwrap();
    }
}
