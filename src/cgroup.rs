use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

pub type DMemLimit = BTreeMap<String, u64>;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CGroup {
    hierarchy_root: PathBuf,
    path: PathBuf,
}

impl CGroup {
    pub fn hierarchy_root(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        if !path.is_absolute() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "the cgroup mount path must be absolute",
            ));
        }
        if !path.is_dir() {
            return Err(io::Error::new(
                ErrorKind::NotFound,
                format!("cgroup mount {} does not exist", path.display()),
            ));
        }

        let path = fs::canonicalize(path)?;
        Ok(Self {
            hierarchy_root: path.clone(),
            path,
        })
    }

    pub fn from_path(hierarchy_root: &Path, path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        if path
            .components()
            .any(|component| component == Component::ParentDir)
        {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("cgroup path {} contains '..'", path.display()),
            ));
        }
        if path != hierarchy_root && !path.starts_with(hierarchy_root) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "cgroup path {} is outside hierarchy {}",
                    path.display(),
                    hierarchy_root.display()
                ),
            ));
        }

        Ok(Self {
            hierarchy_root: hierarchy_root.to_path_buf(),
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_hierarchy_root(&self) -> bool {
        self.path == self.hierarchy_root
    }

    pub fn parent(&self) -> Option<Self> {
        if self.is_hierarchy_root() {
            return None;
        }

        let parent = self.path.parent()?;
        Self::from_path(&self.hierarchy_root, parent.to_path_buf()).ok()
    }

    pub fn children(&self) -> io::Result<Vec<Self>> {
        let mut children = Vec::new();

        for entry in fs::read_dir(&self.path)? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if !file_type.is_dir() {
                continue;
            }

            // Child directories in cgroupfs expose cgroup.procs. This also keeps
            // synthetic test trees and accidental normal subdirectories separate.
            if !entry.path().join("cgroup.procs").exists() {
                continue;
            }

            if let Ok(child) = Self::from_path(&self.hierarchy_root, entry.path()) {
                children.push(child);
            }
        }

        children.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(children)
    }

    pub fn available_controllers(&self) -> io::Result<BTreeSet<String>> {
        read_word_set(self.path.join("cgroup.controllers"))
    }

    pub fn active_controllers(&self) -> io::Result<BTreeSet<String>> {
        read_word_set(self.path.join("cgroup.subtree_control"))
    }

    pub fn subtree_control_owner(&self) -> io::Result<u32> {
        Ok(fs::metadata(self.path.join("cgroup.subtree_control"))?.uid())
    }

    pub fn subtree_control_is_writable(&self) -> bool {
        OpenOptions::new()
            .write(true)
            .open(self.path.join("cgroup.subtree_control"))
            .is_ok()
    }

    pub fn has_local_processes(&self) -> io::Result<bool> {
        Ok(!fs::read_to_string(self.path.join("cgroup.procs"))?
            .trim()
            .is_empty())
    }

    pub fn enable_controller(&self, controller: &str) -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .open(self.path.join("cgroup.subtree_control"))?;
        file.write_all(format!("+{controller}").as_bytes())
    }

    pub fn device_memory_capacity(&self) -> io::Result<DMemLimit> {
        parse_limits_file(self.path.join("dmem.capacity"))
    }

    pub fn device_memory_low(&self) -> io::Result<DMemLimit> {
        parse_limits_file(self.path.join("dmem.low"))
    }

    pub fn write_device_memory_low(&self, limits: &DMemLimit) -> io::Result<bool> {
        let path = self.path.join("dmem.low");
        if self.device_memory_low().as_ref().ok() == Some(limits) {
            return Ok(false);
        }

        let mut contents = String::new();
        for (region, value) in limits {
            contents.push_str(region);
            contents.push(' ');
            if *value == u64::MAX {
                contents.push_str("max");
            } else {
                contents.push_str(&value.to_string());
            }
            contents.push('\n');
        }

        let mut file = OpenOptions::new().write(true).open(path)?;
        file.write_all(contents.as_bytes())?;
        Ok(true)
    }
}

fn read_word_set(path: impl AsRef<Path>) -> io::Result<BTreeSet<String>> {
    Ok(fs::read_to_string(path)?
        .split_whitespace()
        .map(ToOwned::to_owned)
        .collect())
}

fn parse_limits_file(path: impl AsRef<Path>) -> io::Result<DMemLimit> {
    parse_limits(&fs::read_to_string(path)?)
}

fn parse_limits(contents: &str) -> io::Result<DMemLimit> {
    let mut limits = DMemLimit::new();

    for (line_number, line) in contents.lines().enumerate() {
        let mut fields = line.split_whitespace();
        let region = match fields.next() {
            Some(region) => region,
            None => continue,
        };
        let raw_value = fields.next().ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidData,
                format!("missing value on dmem line {}", line_number + 1),
            )
        })?;
        if fields.next().is_some() {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("too many fields on dmem line {}", line_number + 1),
            ));
        }

        let value = if raw_value == "max" {
            u64::MAX
        } else {
            raw_value.parse::<u64>().map_err(|error| {
                io::Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "invalid dmem value {raw_value:?} on line {}: {error}",
                        line_number + 1
                    ),
                )
            })?
        };
        limits.insert(region.to_owned(), value);
    }

    Ok(limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_limits_and_max() {
        let parsed = parse_limits(
            "drm/0000:03:00.0/vram0 8514437120\n\
             drm/0000:03:00.0/stolen max\n",
        )
        .unwrap();

        assert_eq!(parsed["drm/0000:03:00.0/vram0"], 8_514_437_120);
        assert_eq!(parsed["drm/0000:03:00.0/stolen"], u64::MAX);
    }

    #[test]
    fn rejects_malformed_limits() {
        assert!(parse_limits("gpu0 nope\n").is_err());
        assert!(parse_limits("gpu0 1 extra\n").is_err());
        assert!(parse_limits("gpu0\n").is_err());
    }
}
