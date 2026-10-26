// The optional eframe demo embeds the library UI but not the custom GL host;
// those playback hooks are used by `egui-gl-host-proof`.
#[allow(dead_code)]
mod app;
mod catalog;
#[cfg(debug_assertions)]
mod debug_log;
mod device_id;
// The UI modules refer to `crate::input`; the bin must declare it as the lib does.
#[allow(dead_code)]
mod input;
mod parental;
mod saved_servers;
mod settings_state;
mod sign_in;
mod theme;
mod ui;
#[allow(dead_code)]
mod worker;

/// Command-line options. M2 defines one: `--profile <name>`.
#[derive(Debug, Default, PartialEq, Eq)]
struct LaunchOptions {
    profile: Option<String>,
}

/// Read `--profile <name>` or `--profile=<name>`. Any other argument is an error, so a
/// mistyped flag is reported instead of ignored.
fn parse_launch_args<I>(args: I) -> Result<LaunchOptions, String>
where
    I: IntoIterator<Item = String>,
{
    let mut options = LaunchOptions::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--profile" {
            let name = args
                .next()
                .ok_or_else(|| "--profile needs a profile name".to_owned())?;
            options.profile = Some(name);
        } else if let Some(name) = arg.strip_prefix("--profile=") {
            options.profile = Some(name.to_owned());
        } else {
            return Err(format!("unknown argument `{arg}`"));
        }
    }
    Ok(options)
}

fn main() -> eframe::Result {
    let requested = match parse_launch_args(std::env::args().skip(1)) {
        Ok(options) => options.profile,
        Err(message) => {
            eprintln!("jellyfin-kattri: {message}");
            None
        }
    };

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Jellyfin Kattri")
            .with_inner_size([1440.0, 960.0])
            .with_min_inner_size([760.0, 560.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Jellyfin Kattri",
        options,
        Box::new(move |creation_context| {
            let mut app = app::KattriApp::with_launch_profile(
                creation_context.egui_ctx.clone(),
                requested.as_deref(),
            );
            app.offer_launch_picker();
            app.resume_saved_sign_in();
            Ok(Box::new(EframeDemoApp(app)))
        }),
    )
}

struct EframeDemoApp(app::KattriApp);

impl eframe::App for EframeDemoApp {
    fn ui(&mut self, root: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        self.0.ui(root);
    }
}

#[cfg(test)]
mod tests {
    use super::{LaunchOptions, parse_launch_args};

    fn parse(args: &[&str]) -> Result<LaunchOptions, String> {
        parse_launch_args(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn no_arguments_selects_nothing() {
        assert_eq!(parse(&[]), Ok(LaunchOptions { profile: None }));
    }

    #[test]
    fn profile_flag_takes_the_next_argument() {
        assert_eq!(
            parse(&["--profile", "kids"]),
            Ok(LaunchOptions {
                profile: Some("kids".to_owned())
            })
        );
    }

    #[test]
    fn profile_flag_accepts_the_equals_form() {
        assert_eq!(
            parse(&["--profile=kids"]),
            Ok(LaunchOptions {
                profile: Some("kids".to_owned())
            })
        );
    }

    #[test]
    fn profile_flag_without_a_value_is_an_error() {
        assert!(parse(&["--profile"]).is_err());
    }

    #[test]
    fn unknown_argument_is_an_error_that_names_it() {
        assert_eq!(
            parse(&["--profle", "kids"]),
            Err("unknown argument `--profle`".to_owned())
        );
    }
}
