//! Structure-only fixture folders for the M6-16 disc input classifier. No disc media, image
//! content, or drive is used: the files are empty markers and the policy is pure.

use std::fs;
use std::path::{Path, PathBuf};

use kattri_client_core::disc_input::{
    DISC_PLAYBACK_GATE, DISC_RIGHTS_NOTICE, DiscInput, DiscInputError, DiscOpenPlan,
    DiscOpenRefusal, DiscOpenTrigger, DiscPlaybackGate, DiscProbe, DiscProtocol, classify,
    plan_open,
};

/// A unique temporary directory that removes itself when the test ends.
struct FixtureDir(PathBuf);

impl FixtureDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "kattri-disc-input-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create fixture directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, relative: &str, bytes: &[u8]) {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create fixture parent directory");
        }
        fs::write(path, bytes).expect("write fixture marker");
    }
}

impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn open_plan(input: DiscInput) -> DiscOpenPlan {
    plan_open(
        Ok(input),
        DiscOpenTrigger::ExplicitUserAction,
        DiscPlaybackGate::Open,
    )
    .expect("checked input with the gate open")
}

#[test]
fn bluray_fixture_folder_classifies_and_plans_the_bluray_protocol() {
    let disc = FixtureDir::new("bdmv");
    disc.write("BDMV/index.bdmv", b"INDX0200");
    disc.write("BDMV/PLAYLIST/00000.mpls", b"MPLS0200");
    disc.write("BDMV/STREAM/00000.m2ts", b"");

    assert_eq!(
        classify(disc.path(), DiscProbe::from_path(disc.path())),
        Ok(DiscInput::BdmvFolder)
    );
    let selected = disc.path().join("BDMV");
    assert_eq!(
        classify(&selected, DiscProbe::from_path(&selected)),
        Ok(DiscInput::BdmvFolder)
    );

    let plan = open_plan(DiscInput::BdmvFolder);
    assert_eq!(plan.protocol, Some(DiscProtocol::BluRay));
    assert_eq!(plan.load_argument(), Some("bd://"));
    assert_eq!(plan.rights_notice, DISC_RIGHTS_NOTICE);
}

#[test]
fn dvd_fixture_folder_classifies_and_plans_the_dvd_protocol() {
    let disc = FixtureDir::new("video-ts");
    disc.write("VIDEO_TS/VIDEO_TS.IFO", b"DVDVIDEO-VMG");
    disc.write("VIDEO_TS/VTS_01_0.IFO", b"DVDVIDEO-VTS");

    assert_eq!(
        classify(disc.path(), DiscProbe::from_path(disc.path())),
        Ok(DiscInput::VideoTsFolder)
    );
    let selected = disc.path().join("VIDEO_TS");
    assert_eq!(
        classify(&selected, DiscProbe::from_path(&selected)),
        Ok(DiscInput::VideoTsFolder)
    );

    let plan = open_plan(DiscInput::VideoTsFolder);
    assert_eq!(plan.protocol, Some(DiscProtocol::Dvd));
    assert_eq!(plan.protocol.and_then(DiscProtocol::device_option), None);
}

#[test]
fn an_iso_fixture_file_classifies_as_an_image() {
    let dir = FixtureDir::new("iso");
    let iso = dir.path().join("disc.iso");
    fs::write(&iso, b"").expect("write fixture ISO marker");

    assert_eq!(
        classify(&iso, DiscProbe::from_path(&iso)),
        Ok(DiscInput::IsoImage)
    );
    assert_eq!(open_plan(DiscInput::IsoImage).protocol, None);
}

#[test]
fn non_disc_fixtures_are_refused() {
    let empty = FixtureDir::new("empty");
    assert_eq!(
        classify(empty.path(), DiscProbe::from_path(empty.path())),
        Err(DiscInputError::NotDiscStructure)
    );

    let plain = FixtureDir::new("plain");
    plain.write("movie.mkv", b"");
    let movie = plain.path().join("movie.mkv");
    assert_eq!(
        classify(&movie, DiscProbe::from_path(&movie)),
        Err(DiscInputError::NotDiscStructure)
    );

    let missing = empty.path().join("absent.iso");
    assert_eq!(
        classify(&missing, DiscProbe::from_path(&missing)),
        Err(DiscInputError::NotFound)
    );
}

#[test]
fn a_hybrid_fixture_with_both_structures_is_refused() {
    let disc = FixtureDir::new("hybrid");
    disc.write("BDMV/index.bdmv", b"INDX0200");
    disc.write("VIDEO_TS/VIDEO_TS.IFO", b"DVDVIDEO-VMG");
    assert_eq!(
        classify(disc.path(), DiscProbe::from_path(disc.path())),
        Err(DiscInputError::AmbiguousStructure)
    );
}

#[test]
fn the_gate_and_the_explicit_action_are_checked_before_the_path() {
    assert_eq!(DISC_PLAYBACK_GATE, DiscPlaybackGate::Closed);
    assert_eq!(
        plan_open(
            Err(DiscInputError::NotFound),
            DiscOpenTrigger::ExplicitUserAction,
            DISC_PLAYBACK_GATE
        ),
        Err(DiscOpenRefusal::FeatureGated),
        "a closed gate must be reported before the path is inspected"
    );
    assert_eq!(
        plan_open(
            Ok(DiscInput::BdmvFolder),
            DiscOpenTrigger::Automatic,
            DiscPlaybackGate::Open
        ),
        Err(DiscOpenRefusal::AutomaticOpenRefused)
    );
}
