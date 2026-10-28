use std::path::Path;
use tokio::io::AsyncReadExt;

pub(super) const MAX_CUSTOM_BACKDROP_BYTES: u64 = 24 * 1024 * 1024;

pub(super) async fn read_custom_backdrop(path: &Path) -> Option<Vec<u8>> {
    read_with_limit(path, MAX_CUSTOM_BACKDROP_BYTES).await
}

async fn read_with_limit(path: &Path, limit: u64) -> Option<Vec<u8>> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    if !metadata.is_file() || metadata.len() > limit {
        return None;
    }
    let file = tokio::fs::File::open(path).await.ok()?;
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).ok()?);
    file.take(limit + 1).read_to_end(&mut bytes).await.ok()?;
    (bytes.len() as u64 <= limit).then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_backdrop_reads_are_bounded_and_regular_files_only() {
        let root =
            std::env::temp_dir().join(format!("kattri-custom-backdrop-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create test directory");
        let valid = root.join("valid.jpg");
        let oversized = root.join("oversized.jpg");
        std::fs::write(&valid, b"image").expect("write valid fixture");
        std::fs::write(&oversized, vec![0; 65]).expect("write oversized fixture");
        assert_eq!(read_with_limit(&valid, 64).await, Some(b"image".to_vec()));
        assert_eq!(read_with_limit(&root, 64).await, None);
        assert_eq!(read_with_limit(&root.join("missing.jpg"), 64).await, None);
        assert_eq!(read_with_limit(&oversized, 64).await, None);
        std::fs::remove_dir_all(root).expect("remove test directory");
    }
}
