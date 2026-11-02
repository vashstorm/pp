//! Filesystem interaction in the shell.

use std::path::{Path, PathBuf};

use normalize_path::NormalizePath as _;

use crate::{
    ExecutionParameters, ShellFd,
    env::{EnvironmentLookup, EnvironmentScope},
    error, openfiles, pathsearch,
    sys::{fs::PathExt as _, users},
    variables,
};

/// Describes how a redirect target file should be created.
///
/// A shell's virtual umask, rather than the host process's, determines the
/// permissions of files the shell creates. Because the host umask may be
/// stricter than the shell's, newly created files are always re-chmodded to the
/// exact target mode.
#[derive(Clone, Copy, Debug)]
pub struct RedirectCreation {
    /// Permission bits for a newly created file (`0o666 & !umask`).
    pub mode: u32,
    /// Whether creation must fail when the file already exists (`noclobber`).
    pub exclusive: bool,
}

impl<SE: crate::extensions::ShellExtensions> crate::Shell<SE> {
    /// Sets the shell's current working directory to the given path.
    ///
    /// # Arguments
    ///
    /// * `target_dir` - The path to set as the working directory.
    pub fn set_working_dir(&mut self, target_dir: impl AsRef<Path>) -> Result<(), error::Error> {
        let abs_path = self.absolute_path(target_dir.as_ref());

        match std::fs::metadata(&abs_path) {
            Ok(m) => {
                if !m.is_dir() {
                    return Err(error::ErrorKind::NotADirectory(abs_path).into());
                }
            }
            Err(e) => {
                return Err(e.into());
            }
        }

        // Normalize the path (but don't canonicalize it).
        let cleaned_path = abs_path.normalize();

        let pwd = cleaned_path.to_string_lossy().to_string();

        self.env.update_or_add(
            "PWD",
            variables::ShellValueLiteral::Scalar(pwd),
            |_| Ok(()),
            EnvironmentLookup::Anywhere,
            EnvironmentScope::Global,
        )?;
        let oldpwd = std::mem::replace(self.working_dir_mut(), cleaned_path);

        self.env.update_or_add(
            "OLDPWD",
            variables::ShellValueLiteral::Scalar(oldpwd.to_string_lossy().to_string()),
            |_| Ok(()),
            EnvironmentLookup::Anywhere,
            EnvironmentScope::Global,
        )?;

        Ok(())
    }

    /// Tilde-shortens the given string, replacing the user's home directory with a tilde.
    ///
    /// # Arguments
    ///
    /// * `s` - The string to shorten.
    pub fn tilde_shorten(&self, s: String) -> String {
        if let Some(home_dir) = self.home_dir()
            && let Some(stripped) = s.strip_prefix(home_dir.to_string_lossy().as_ref())
        {
            return format!("~{stripped}");
        }
        s
    }

    /// Returns the shell's current home directory, if available.
    pub(crate) fn home_dir(&self) -> Option<PathBuf> {
        if let Some(home) = self.env.get_str("HOME", self) {
            Some(PathBuf::from(home.to_string()))
        } else {
            // HOME isn't set, so let's sort it out ourselves.
            users::get_current_user_home_dir()
        }
    }

    /// Finds executables in the shell's current default PATH, matching the given glob pattern.
    ///
    /// # Arguments
    ///
    /// * `required_glob_pattern` - The glob pattern to match against.
    pub fn find_executables_in_path<'a>(
        &'a self,
        filename: &'a str,
    ) -> impl Iterator<Item = PathBuf> + 'a {
        let path_var = self.env.get_str("PATH", self).unwrap_or_default();
        let paths = crate::sys::fs::split_paths(path_var.as_ref());

        pathsearch::search_for_executable(paths, filename)
    }

    /// Finds executables in the shell's current default PATH, with filenames matching the
    /// given prefix.
    ///
    /// # Arguments
    ///
    /// * `filename_prefix` - The prefix to match against executable filenames.
    pub fn find_executables_in_path_with_prefix(
        &self,
        filename_prefix: &str,
        case_insensitive: bool,
    ) -> impl Iterator<Item = PathBuf> {
        let path_var = self.env.get_str("PATH", self).unwrap_or_default();
        let paths = crate::sys::fs::split_paths(path_var.as_ref());

        pathsearch::search_for_executable_with_prefix(paths, filename_prefix, case_insensitive)
    }

    /// Determines whether the given filename is the name of an executable in one of the
    /// directories in the shell's current PATH. If found, returns the path.
    ///
    /// # Arguments
    ///
    /// * `candidate_name` - The name of the file to look for.
    pub fn find_first_executable_in_path<S: AsRef<str>>(
        &self,
        candidate_name: S,
    ) -> Option<PathBuf> {
        let path = self.env_str("PATH").unwrap_or_default();
        for one_dir in crate::sys::fs::split_paths(path.as_ref()) {
            let candidate_path = one_dir.join(candidate_name.as_ref());
            if candidate_path.executable() {
                return Some(candidate_path);
            }
        }
        None
    }

    /// Uses the shell's hash-based path cache to check whether the given filename is the name
    /// of an executable in one of the directories in the shell's current PATH. If found,
    /// ensures the path is in the cache and returns it.
    ///
    /// # Arguments
    ///
    /// * `candidate_name` - The name of the file to look for.
    pub fn find_first_executable_in_path_using_cache<S: AsRef<str>>(
        &mut self,
        candidate_name: S,
    ) -> Option<PathBuf>
    where
        String: From<S>,
    {
        if let Some(cached_path) = self.program_location_cache.get(&candidate_name) {
            Some(cached_path)
        } else if let Some(found_path) = self.find_first_executable_in_path(&candidate_name) {
            self.program_location_cache
                .set(candidate_name, found_path.clone());
            Some(found_path)
        } else {
            None
        }
    }

    /// Gets the absolute form of the given path.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to get the absolute form of.
    pub fn absolute_path(&self, path: impl AsRef<Path>) -> PathBuf {
        let path = path.as_ref();
        if path.as_os_str().is_empty() || path.is_absolute() {
            path.to_owned()
        } else {
            self.working_dir().join(path)
        }
    }

    /// Opens the given file, using the context of this shell and the provided execution parameters.
    ///
    /// # Arguments
    ///
    /// * `options` - The options to use opening the file.
    /// * `path` - The path to the file to open; may be relative to the shell's working directory.
    /// * `params` - Execution parameters.
    pub(crate) fn open_file(
        &self,
        options: &std::fs::OpenOptions,
        path: impl AsRef<Path>,
        params: &ExecutionParameters,
    ) -> Result<openfiles::OpenFile, std::io::Error> {
        // Give platform-specific code a chance to handle special files
        // (e.g. /dev/null on Windows, which needs to open NUL instead).
        // This is checked before absolute_path so that paths like /dev/null
        // are intercepted on platforms where they aren't valid native paths.
        if let Some(result) = crate::sys::fs::try_open_special_file(path.as_ref()) {
            return result.map(openfiles::OpenFile::from);
        }

        if let Some(result) = self.open_logical_fd_path(path.as_ref(), params) {
            return result;
        }

        Ok(options.open(self.absolute_path(path.as_ref()))?.into())
    }

    /// Opens a redirect target, honoring the shell's virtual umask.
    ///
    /// Newly created files are opened exclusively first so that their exact
    /// permissions can be applied without ever touching the host's umask, and
    /// so that a `noclobber` failure is reported as such. Existing files are
    /// opened with their normal truncate/append flags and are never chmodded.
    ///
    /// # Arguments
    ///
    /// * `options` - The options describing how an existing file is opened.
    /// * `path` - The redirect target path.
    /// * `params` - Execution parameters.
    /// * `creation` - How to create a missing file, or `None` to avoid creating one.
    pub(crate) fn open_redirect_file(
        &self,
        options: &std::fs::OpenOptions,
        path: impl AsRef<Path>,
        params: &ExecutionParameters,
        creation: Option<RedirectCreation>,
    ) -> Result<openfiles::OpenFile, std::io::Error> {
        let Some(creation) = creation else {
            return self.open_file(options, path, params);
        };

        let path = path.as_ref();

        if let Some(result) = crate::sys::fs::try_open_special_file(path) {
            return result.map(openfiles::OpenFile::from);
        }

        if let Some(result) = self.open_logical_fd_path(path, params) {
            return result;
        }

        let path_to_open = self.absolute_path(path);

        let mut create_options = options.clone();
        create_options.create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            create_options.mode(creation.mode);
        }

        match create_options.open(&path_to_open) {
            Ok(file) => {
                Self::apply_exact_mode(&file, creation.mode)?;
                Ok(file.into())
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists && !creation.exclusive => {
                // The file already exists (or appeared in the meantime): open it
                // with the caller's normal flags without changing its permissions.
                Ok(options.open(&path_to_open)?.into())
            }
            Err(err) => Err(err),
        }
    }

    /// Resolves a `/dev/std*`, `/dev/fd/*` or `/proc/self/fd/*` path to the
    /// shell's logical descriptor table instead of the host's real descriptors.
    ///
    /// Returns `None` when the path does not name a descriptor, leaving the
    /// caller to fall back to an ordinary filesystem open.
    fn open_logical_fd_path(
        &self,
        path: &Path,
        params: &ExecutionParameters,
    ) -> Option<Result<openfiles::OpenFile, std::io::Error>> {
        let fd = logical_fd_for_special_path(path)?;

        if let Some(open_file) = params.try_fd(self, fd) {
            return Some(open_file.try_clone());
        }

        if self.is_embedded() {
            return Some(Err(std::io::Error::from(std::io::ErrorKind::NotFound)));
        }

        None
    }

    /// Applies exactly the requested permission bits to a freshly created file.
    #[cfg(unix)]
    fn apply_exact_mode(file: &std::fs::File, mode: u32) -> Result<(), std::io::Error> {
        use std::os::fd::AsFd as _;

        let bits = libc::mode_t::try_from(mode & 0o777).unwrap_or(0);
        let mode = nix::sys::stat::Mode::from_bits_truncate(bits);
        nix::sys::stat::fchmod(file.as_fd(), mode)?;

        Ok(())
    }

    /// Applies exactly the requested permission bits to a freshly created file.
    #[cfg(not(unix))]
    fn apply_exact_mode(_file: &std::fs::File, _mode: u32) -> Result<(), std::io::Error> {
        Ok(())
    }

    /// Replaces the shell's currently configured open files with the given set.
    /// Typically only used by exec-like builtins.
    ///
    /// # Arguments
    ///
    /// * `open_files` - The new set of open files to use.
    pub fn replace_open_files(
        &mut self,
        open_fds: impl Iterator<Item = (ShellFd, openfiles::OpenFile)>,
    ) {
        self.open_files = openfiles::OpenFiles::from(open_fds);
    }

    pub(crate) const fn persistent_open_files(&self) -> &openfiles::OpenFiles {
        &self.open_files
    }
}

/// Returns the logical descriptor that a well-known path alias refers to.
fn logical_fd_for_special_path(path: &Path) -> Option<ShellFd> {
    match path.to_str()? {
        "/dev/stdin" => return Some(0),
        "/dev/stdout" => return Some(1),
        "/dev/stderr" => return Some(2),
        other => {
            let fd = other
                .strip_prefix("/dev/fd/")
                .or_else(|| other.strip_prefix("/proc/self/fd/"))?;

            fd.parse::<ShellFd>().ok()
        }
    }
}
