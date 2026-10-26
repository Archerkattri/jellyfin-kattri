//! M2-15 screenshot command: builds the output path with the client-core screenshot rules and
//! queues mpv `screenshot-to-file`. The path and command are built by pure functions; only
//! [`MpvSession::screenshot_to_file`] touches the owner-thread queue.

use std::{error::Error, fmt, path::Path};

use kattri_client_core::screenshot::{
    NameError, PathError, ScreenshotContext, ScreenshotFormat, ScreenshotMode, TemplateError,
    screenshot_command, screenshot_path, screenshot_stem, unique_file_name,
};

use crate::{CommandId, MpvError, MpvSession};

/// Screenshot coverage used when the caller passes this constant to [`MpvSession::screenshot_to_file`].
///
/// provisional: owner to confirm (subtitles vs video). `Subtitles` is the legacy parity choice
/// (keeps subtitles, omits the OSD); `Video` drops subtitles too. [UNCONFIRMED: mpv flag names
/// `subtitles` and `video` are not checked against a real mpv run.]
pub const PROVISIONAL_SCREENSHOT_MODE: ScreenshotMode = ScreenshotMode::Subtitles;

/// A screenshot request was refused before queueing, or queueing failed.
#[derive(Debug)]
pub enum ScreenshotError {
    /// The filename template is invalid.
    Template(TemplateError),
    /// No free file name was found for the stem.
    Name(NameError),
    /// The file name or path is unsafe, or the command path is refused.
    Path(PathError),
    /// The resulting path is not valid UTF-8, so it cannot be sent as an mpv argument.
    NonUtf8Path,
    /// The owner-thread command queue rejected the request.
    Mpv(MpvError),
}

impl fmt::Display for ScreenshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Template(error) => {
                write!(formatter, "screenshot name template is invalid: {error:?}")
            }
            Self::Name(error) => write!(formatter, "no free screenshot file name: {error:?}"),
            Self::Path(error) => write!(formatter, "screenshot path is unsafe: {error:?}"),
            Self::NonUtf8Path => formatter.write_str("screenshot path is not valid UTF-8"),
            Self::Mpv(error) => write!(formatter, "could not queue screenshot command: {error}"),
        }
    }
}

impl Error for ScreenshotError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Mpv(error) => Some(error),
            Self::Template(_) | Self::Name(_) | Self::Path(_) | Self::NonUtf8Path => None,
        }
    }
}

/// Pure part of M2-15: the mpv `screenshot-to-file` arguments for one capture.
///
/// `existing_names` lists the file names already in `directory`, so a collision gets a `-2`
/// style suffix. The caller reads the directory; this function does no I/O.
fn screenshot_arguments(
    directory: &Path,
    template: &str,
    context: &ScreenshotContext<'_>,
    existing_names: &[String],
    mode: ScreenshotMode,
) -> Result<[String; 2], ScreenshotError> {
    let stem = screenshot_stem(template, context).map_err(ScreenshotError::Template)?;
    let file_name = unique_file_name(&stem, ScreenshotFormat::Png, existing_names)
        .map_err(ScreenshotError::Name)?;
    let path = screenshot_path(directory, &file_name).map_err(ScreenshotError::Path)?;
    let path_text = path.to_str().ok_or(ScreenshotError::NonUtf8Path)?;
    let command = screenshot_command(path_text, mode).map_err(ScreenshotError::Path)?;
    Ok([command.args[0].clone(), command.args[1].clone()])
}

impl MpvSession {
    /// Queue mpv `screenshot-to-file` for one capture (M2-15). The output path is
    /// `directory` joined with a sanitized name from `template`, with the PNG extension. Pass
    /// [`PROVISIONAL_SCREENSHOT_MODE`] unless the owner has chosen otherwise. Does not wait for
    /// libmpv or for the file to be written. [UNCONFIRMED: the `screenshot-to-file` command name.]
    pub fn screenshot_to_file(
        &self,
        directory: &Path,
        template: &str,
        context: &ScreenshotContext<'_>,
        existing_names: &[String],
        mode: ScreenshotMode,
    ) -> Result<CommandId, ScreenshotError> {
        let arguments = screenshot_arguments(directory, template, context, existing_names, mode)?;
        self.command(&["screenshot-to-file", &arguments[0], &arguments[1]])
            .map_err(ScreenshotError::Mpv)
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, time::Duration};

    use kattri_client_core::screenshot::{PathError, ScreenshotContext, ScreenshotMode};

    use super::{PROVISIONAL_SCREENSHOT_MODE, ScreenshotError, screenshot_arguments};

    fn context(position_secs: u64) -> ScreenshotContext<'static> {
        ScreenshotContext {
            file_stem: "movie",
            position: Duration::from_secs(position_secs),
            counter: 1,
        }
    }

    fn expected_path(directory: &Path, name: &str) -> String {
        directory.join(name).to_str().unwrap().to_owned()
    }

    #[test]
    fn provisional_mode_is_subtitles_until_the_owner_confirms() {
        assert_eq!(PROVISIONAL_SCREENSHOT_MODE, ScreenshotMode::Subtitles);
    }

    #[test]
    fn default_template_builds_a_png_path_and_the_mode_flag() {
        let directory = Path::new("shots");
        let [path, mode] =
            screenshot_arguments(directory, "", &context(5), &[], PROVISIONAL_SCREENSHOT_MODE)
                .unwrap();
        assert_eq!(
            path,
            expected_path(directory, "kattri-movie-00.00.05.000.png")
        );
        assert_eq!(mode, "subtitles");
    }

    #[test]
    fn video_mode_is_passed_through_when_the_caller_chooses_it() {
        let [_, mode] = screenshot_arguments(
            Path::new("shots"),
            "",
            &context(0),
            &[],
            ScreenshotMode::Video,
        )
        .unwrap();
        assert_eq!(mode, "video");
    }

    #[test]
    fn collisions_get_a_numeric_suffix() {
        let directory = Path::new("shots");
        let existing = vec!["kattri-movie-00.00.05.000.png".to_owned()];
        let [path, _] = screenshot_arguments(
            directory,
            "",
            &context(5),
            &existing,
            PROVISIONAL_SCREENSHOT_MODE,
        )
        .unwrap();
        assert_eq!(
            path,
            expected_path(directory, "kattri-movie-00.00.05.000-2.png")
        );
    }

    #[test]
    fn unknown_template_placeholder_is_refused_before_any_queueing() {
        assert!(matches!(
            screenshot_arguments(
                Path::new("shots"),
                "bad-%x",
                &context(0),
                &[],
                PROVISIONAL_SCREENSHOT_MODE,
            ),
            Err(ScreenshotError::Template(_))
        ));
    }

    #[test]
    fn path_separators_in_the_template_stay_inside_the_directory() {
        let directory = Path::new("shots");
        let [path, _] = screenshot_arguments(
            directory,
            "sub/dir-%f",
            &context(0),
            &[],
            PROVISIONAL_SCREENSHOT_MODE,
        )
        .unwrap();
        assert_eq!(path, expected_path(directory, "sub_dir-movie.png"));
    }

    #[test]
    fn command_path_starting_with_dash_is_refused() {
        // The joined path starts with `-`, so mpv would read it as an option. It is refused.
        assert!(matches!(
            screenshot_arguments(
                Path::new("-shots"),
                "x",
                &context(0),
                &[],
                PROVISIONAL_SCREENSHOT_MODE,
            ),
            Err(ScreenshotError::Path(PathError::UnsafeCommandPath))
        ));
    }
}
