use std::{
    fs,
    io::{self, Write},
    path::Path,
};

use tempfile::NamedTempFile;

pub const SKILL: &str = include_str!("../plugins/bettercoding/skills/operate/SKILL.md");
const MANIFEST: &str = include_str!("../plugins/bettercoding/.claude-plugin/plugin.json");

pub fn claude_plugin_args(data_dir: &Path) -> io::Result<[String; 2]> {
    let plugin_dir = data_dir.join("bundled-plugins/bettercoding");
    for (relative_path, content) in [
        ("skills/operate/SKILL.md", SKILL),
        (".claude-plugin/plugin.json", MANIFEST),
    ] {
        let path = plugin_dir.join(relative_path);
        if fs::read(&path).is_ok_and(|bytes| bytes == content.as_bytes()) {
            continue;
        }
        let parent = path.parent().expect("bundled file has a parent");
        fs::create_dir_all(parent)?;
        // Concurrent launches or a relaunch after an upgrade must see complete files.
        let mut file = NamedTempFile::new_in(parent)?;
        file.write_all(content.as_bytes())?;
        file.persist(path)?;
    }

    let plugin_dir = plugin_dir.into_os_string().into_string().map_err(|path| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Claude plugin path is not UTF-8: {}",
                Path::new(&path).display()
            ),
        )
    })?;
    Ok(["--plugin-dir".into(), plugin_dir])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_and_refreshes_the_bundled_plugin() {
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path().join("data with spaces");
        let [flag, path] = claude_plugin_args(&data_dir).unwrap();
        assert_eq!(flag, "--plugin-dir");
        let plugin = Path::new(&path);
        assert!(plugin.starts_with(&data_dir));
        assert_eq!(
            fs::read_to_string(plugin.join(".claude-plugin/plugin.json")).unwrap(),
            MANIFEST,
        );
        let skill = plugin.join("skills/operate/SKILL.md");
        assert_eq!(fs::read_to_string(&skill).unwrap(), SKILL);

        fs::write(&skill, "Old skill from a previous release").unwrap();
        assert_eq!(claude_plugin_args(&data_dir).unwrap(), [flag, path]);
        assert_eq!(fs::read_to_string(&skill).unwrap(), SKILL);
    }
}
