use std::{
    io::Write,
    path::Path,
};

pub fn extract(iso_path: &str, dest: &Path) -> std::io::Result<usize> {
    let img = std::fs::File::open(iso_path)?;

    let mut dev = xdvdfs::blockdev::OffsetWrapper::new(img)
        .map_err(|e: xdvdfs::util::Error<std::io::Error>| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{:?}", e))
        })?;

    let volume = xdvdfs::read::read_volume(&mut dev)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{:?}", e)))?;

    let tree = volume
        .root_table
        .file_tree(&mut dev)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{:?}", e)))?;

    let mut count = 0usize;

    for (parent, node) in &tree {
        let name = node
            .name_str()
            .map_err(|e: xdvdfs::util::Error<std::io::Error>| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{:?}", e))
            })?;

        let rel_path = if parent.is_empty() {
            name.to_string()
        } else {
            format!("{}/{}", parent.trim_start_matches('/'), name)
        };

        let out_path = dest.join(&rel_path);

        if node.node.dirent.is_directory() {
            std::fs::create_dir_all(&out_path)?;
        } else {
            if let Some(parent_dir) = out_path.parent() {
                std::fs::create_dir_all(parent_dir)?;
            }

            let data = node
                .node
                .dirent
                .read_data_all(&mut dev)
                .map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("read_data_all for {}: {:?}", rel_path, e),
                    )
                })?;

            let mut out_file = std::fs::File::create(&out_path)?;
            out_file.write_all(&data)?;
            count += 1;
        }
    }

    Ok(count)
}
