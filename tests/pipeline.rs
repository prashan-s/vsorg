//! End-to-end tests over a fixture tree whose JSON mirrors a real VS Code 1.133.0 install:
//! mixed-case marketplace IDs, a built-in profile sharing all content with Default, an extension
//! that lives only in a named profile, an extension pack, and an orphaned folder.

use std::collections::BTreeSet;
use std::path::PathBuf;

use vscode_organizer::export;
use vscode_organizer::manifest::Manifest;
use vscode_organizer::paths::{Flavor, Layout};
use vscode_organizer::plan::{self, Action};
use vscode_organizer::state::State;
use vscode_organizer::store::Inventory;

fn layout() -> Layout {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    Layout::from_roots(Flavor::Stable, root.join("user"), root.join("extensions"))
}

fn load() -> (State, Inventory) {
    let l = layout();
    let state = State::load(&l).expect("storage.json");
    let inv = Inventory::load(&l, &state).expect("inventory");
    (state, inv)
}

fn ids(inv: &Inventory, profile: &str) -> BTreeSet<String> {
    inv.get(profile).expect("profile").ids()
}

#[test]
fn reads_default_and_named_profiles_from_their_separate_locations() {
    let (_, inv) = load();

    // Default's manifest lives beside the binaries, not under User/profiles.
    assert_eq!(
        ids(&inv, "Default"),
        BTreeSet::from_iter([
            "eamodio.gitlens".to_string(),
            "esbenp.prettier-vscode".to_string(),
            "vscjava.vscode-java-pack".to_string(),
        ]),
        "mixed-case `EsBenP.Prettier-VSCode` must normalise"
    );

    assert_eq!(
        ids(&inv, "Node.js"),
        BTreeSet::from_iter(["esbenp.prettier-vscode".to_string(), "golang.go".to_string()])
    );
}

#[test]
fn a_profile_sharing_extensions_with_default_reports_defaults_set() {
    let (_, inv) = load();
    // Agents sets useDefaultFlags.extensions, so its own manifest is inert.
    assert_eq!(ids(&inv, "Agents"), ids(&inv, "Default"));
}

#[test]
fn default_is_not_a_superset_of_named_profiles() {
    let (_, inv) = load();
    // golang.go is on disk and in Node.js, but absent from Default — the asymmetry that makes
    // "installed" and "in Default's manifest" different questions.
    assert!(!ids(&inv, "Default").contains("golang.go"));
    assert!(inv.on_disk.contains("golang.go"));
    assert!(inv.referenced().contains("golang.go"));
}

#[test]
fn orphans_and_packs_are_detected_from_the_on_disk_scan() {
    let (_, inv) = load();

    assert_eq!(inv.orphans(), BTreeSet::from_iter(["orphan.leftover".to_string()]));
    assert!(inv.dangling().is_empty(), "fixture references nothing missing from disk");

    let members = &inv.packs["vscjava.vscode-java-pack"];
    assert_eq!(members, &["redhat.java", "vscjava.vscode-java-debug", "vscjava.vscode-maven"]);
}

#[test]
fn undeclared_pack_members_are_reported_only_for_declared_packs() {
    let (_, inv) = load();

    let declared: BTreeSet<String> =
        ["vscjava.vscode-java-pack", "redhat.java"].iter().map(|s| s.to_string()).collect();
    let missing = inv.unpinned_pack_members(&declared);
    assert_eq!(
        missing["vscjava.vscode-java-pack"],
        vec!["vscjava.vscode-java-debug", "vscjava.vscode-maven"]
    );

    // Pack absent from the manifest: nothing to pin.
    let declared: BTreeSet<String> = ["redhat.java"].iter().map(|s| s.to_string()).collect();
    assert!(inv.unpinned_pack_members(&declared).is_empty());
}

#[test]
fn a_manifest_derived_from_the_install_plans_to_nothing() {
    // The property that makes `vsorg init` trustworthy as a starting point.
    let (state, inv) = load();
    let derived = export::derive_manifest(&state, &inv);
    let p = plan::build(&derived, &state, &inv, None);
    assert!(p.is_empty(), "expected no drift, got: {:?}", p.actions);
}

#[test]
fn derived_manifest_survives_a_toml_round_trip() {
    let (state, inv) = load();
    let derived = export::derive_manifest(&state, &inv);
    let text = derived.to_toml().expect("serialise");

    let reparsed: Manifest = toml::from_str(&text).expect("reparse");
    let p = plan::build(&reparsed, &state, &inv, None);
    assert!(p.is_empty(), "round-tripped manifest drifted: {:?}", p.actions);
}

#[test]
fn a_fresh_partition_creates_profiles_and_targets_default_by_its_real_name() {
    let m: Manifest = toml::from_str(
        r#"
        base = ["eamodio.gitlens"]

        [profiles.default]
        extensions = []
        prune = true

        [profiles.web]
        extensions = ["esbenp.prettier-vscode"]

        [profiles."node.js"]
        extensions = ["golang.go"]
    "#,
    )
    .unwrap();

    let (state, inv) = load();
    let p = plan::build(&m, &state, &inv, None);

    // web is new; default and Node.js already exist.
    let creates: Vec<&str> = p
        .actions
        .iter()
        .filter(|a| matches!(a, Action::CreateProfile { .. }))
        .map(|a| a.profile())
        .collect();
    assert_eq!(creates, vec!["web"]);

    // `code --profile` matches exactly: manifest keys must resolve to live names.
    let targets: BTreeSet<&str> = p.actions.iter().map(|a| a.profile()).collect();
    assert!(targets.contains("Default"), "got {targets:?}");
    assert!(targets.contains("Node.js"), "got {targets:?}");
    assert!(!targets.contains("default"));
    assert!(!targets.contains("node.js"));

    // Default keeps base and sheds the rest.
    let pruned: BTreeSet<&str> = p
        .actions
        .iter()
        .filter_map(|a| match a {
            Action::Uninstall { profile, id } if profile == "Default" => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        pruned,
        BTreeSet::from_iter(["esbenp.prettier-vscode", "vscjava.vscode-java-pack"])
    );
    assert!(!pruned.contains("eamodio.gitlens"), "base must survive the prune");

    // Ordering: create, then install, then uninstall.
    let kinds: Vec<u8> = p
        .actions
        .iter()
        .map(|a| match a {
            Action::CreateProfile { .. } => 0,
            Action::Install { .. } => 1,
            Action::Uninstall { .. } => 2,
            Action::Manual { .. } => 3,
        })
        .collect();
    assert!(kinds.windows(2).all(|w| w[0] <= w[1]), "out of order: {kinds:?}");
}

#[test]
fn profiles_absent_from_the_manifest_are_reported_and_left_alone() {
    let m: Manifest = toml::from_str(r#"[profiles.default]"#).unwrap();
    let (state, inv) = load();
    let p = plan::build(&m, &state, &inv, None);

    assert_eq!(p.unmanaged, vec!["Node.js"], "builtin Agents must not be listed");
    assert!(p.actions.iter().all(|a| a.profile() == "Default"));
}

#[test]
fn extension_metadata_is_harvested_for_the_classifier() {
    let (_, inv) = load();

    let go = &inv.facts["golang.go"];
    assert_eq!(go.display_name.as_deref(), Some("Go"));
    assert_eq!(go.categories, vec!["Programming Languages", "Debuggers"]);
    assert_eq!(go.languages, vec!["go", "gotmpl"]);
    assert_eq!(go.debuggers, vec!["go"]);
    // `"keywords": null` appears in real manifests and must not take the record down with it.
    assert!(go.keywords.is_empty());

    let pack = &inv.facts["vscjava.vscode-java-pack"];
    assert_eq!(pack.pack, vec!["redhat.java", "vscjava.vscode-java-debug", "vscjava.vscode-maven"]);
    assert_eq!(pack.depends, vec!["redhat.java"]);

    // A package.json with nothing but identity still yields a record, so the ID is never lost.
    assert!(inv.facts.contains_key("orphan.leftover"));
    assert!(inv.facts["orphan.leftover"].display_name.is_none());
}

#[test]
fn classify_validates_a_response_against_the_real_install() {
    use vscode_organizer::classify;

    let (_, inv) = load();
    let opts = classify::Options { allow_unassigned: true, ..Default::default() };

    // The prompt must describe every installed extension, or the model cannot return a partition.
    let prompt = classify::build_prompt(&inv, &opts);
    for id in &inv.on_disk {
        assert!(prompt.contains(id.as_str()), "prompt omitted {id}");
    }
    assert!(prompt.contains("languages: go, gotmpl"));
    assert!(prompt.contains("packMembers: redhat.java"));

    // A response naming something that is not installed is rejected outright.
    let bogus = r#"{"base":[],"profiles":{"x":{"extensions":["totally.invented"]}}}"#;
    let err = classify::ingest(bogus, &inv, &opts).unwrap_err().to_string();
    assert!(err.contains("not installed"), "{err}");

    // A well-formed one becomes a manifest that plans cleanly.
    let good = format!(
        r#"{{"base":["eamodio.gitlens"],"profiles":{{"go":{{"extensions":{}}}}}}}"#,
        serde_json::to_string(
            &inv.on_disk.iter().filter(|i| *i != "eamodio.gitlens").collect::<Vec<_>>()
        )
        .unwrap()
    );
    let (m, report) = classify::ingest(&good, &inv, &opts).unwrap();
    assert!(report.unassigned.is_empty());
    assert!(m.profiles.contains_key("default"), "Default is always synthesised");
    assert_eq!(m.base, vec!["eamodio.gitlens"]);
    // Every installed extension ends up somewhere.
    assert_eq!(m.all_ids(), inv.on_disk);
}

#[test]
fn workspace_bindings_resolve_to_profile_locations() {
    let (state, _) = load();
    assert_eq!(state.workspaces_for("-529b84bd"), vec!["file:///Users/x/svc-api"]);
    assert_eq!(state.workspaces_for("__default__profile__").len(), 2);
}
