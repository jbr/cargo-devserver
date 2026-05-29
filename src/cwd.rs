use std::{
    ffi::OsStr,
    fmt::{self, Display, Formatter},
    ops::Deref,
    path::{Path, PathBuf},
    str::FromStr,
};

use cargo_metadata::camino::Utf8Path;

/// A canonicalized working directory.
///
/// Canonicalizing at parse time means every later comparison (notably against
/// the paths cargo reports) is apples-to-apples, and a nonexistent `--cwd`
/// fails fast with a clear error instead of surfacing later.
#[derive(Debug, Clone)]
pub(crate) struct Cwd(PathBuf);

impl FromStr for Cwd {
    type Err = std::io::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(PathBuf::from(s).canonicalize()?))
    }
}

impl Default for Cwd {
    fn default() -> Self {
        Self(
            std::env::current_dir()
                .and_then(|cwd| cwd.canonicalize())
                .expect("could not determine the current working directory"),
        )
    }
}

impl Deref for Cwd {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Display for Cwd {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.to_string_lossy())
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
