use std::{
    ffi::OsStr,
    ops::Deref,
    path::{Path, PathBuf},
    str::FromStr,
};

use cargo_metadata::camino::Utf8Path;

#[derive(Debug, Clone)]
pub(crate) struct Cwd(PathBuf);
impl FromStr for Cwd {
    type Err = std::io::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(PathBuf::from_str(s).unwrap().canonicalize()?))
    }
}
impl Deref for Cwd {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl Default for Cwd {
    fn default() -> Self {
        Self(std::env::current_dir().unwrap().canonicalize().unwrap())
    }
}
impl ToString for Cwd {
    fn to_string(&self) -> String {
        self.0.to_string_lossy().to_string()
    }
}
impl AsRef<OsStr> for Cwd {
    fn as_ref(&self) -> &OsStr {
        self.0.as_ref()
    }
}
impl AsRef<Path> for Cwd {
    fn as_ref(&self) -> &Path {
        self.0.as_ref()
    }
}
impl PartialEq<Cwd> for &Utf8Path {
    fn eq(&self, other: &Cwd) -> bool {
        self.eq(&other.0)
    }
}
impl From<&OsStr> for Cwd {
    fn from(value: &OsStr) -> Self {
        Self(PathBuf::from(value).canonicalize().unwrap())
    }
}
