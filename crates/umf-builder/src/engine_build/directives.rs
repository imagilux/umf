//! Per-directive dispatch, the metadata/runtime handlers, and the shared
//! types they ride on.
//!
//! [`apply_directive`] is the single entry point [`super::build_one_stage`]
//! calls per AST directive; it fans out to the RUN handler ([`super::run`]),
//! the ADD handlers ([`super::add`]), and the metadata handlers below. RUN and
//! ADD synthesise layers (via the engine or a staged upper-dir); the metadata
//! directives mutate the in-progress image-config in place.

use std::collections::BTreeMap;
use std::path::Path;

use tempfile::TempDir;
use umf_core::ast::{
    Arg, Cmd, CmdForm, Directive, EntrypointInit, Env, Expose, ExposeProtocol, Label, Shell,
    Stopsignal, User, Volume, Workdir,
};
use umf_engine::backends::LibcontainerRuntime;
use umf_engine::bundle::Bundle;
use umf_engine::overlay::PersistedUpper;
use umf_oci::image::{LayerCompression, LayerSource};
use umf_oci::registry::ImageLayout;
use umf_oci::registry::layout::sha256_digest;

use super::cache::{CachedStep, StepCache};
use super::fetch::FetchedUrl;
use super::secrets::ResolvedSecrets;
use super::state::BuildState;
use super::{EngineBuildError, add, run};

/// Map a [`Directive`] to the coarse [`umf_engine::StepKind`] the
/// engine's hook surface speaks. Used by [`super::build_one_stage`] to
/// populate the `StepInfo` before each `apply_directive`.
pub(crate) fn classify_directive(d: &Directive) -> umf_engine::StepKind {
    use umf_engine::StepKind;
    match d {
        Directive::Run(_) => StepKind::Run,
        Directive::Add(_) => StepKind::Add,
        _ => StepKind::Metadata,
    }
}

/// One-line summary of a directive, suitable for the `umf debug
/// build` REPL prompt ("Step 3/7: RUN apt-get install nginx"). Kept
/// short — we truncate long RUN commands so a 200-character one-liner
/// doesn't blow out the prompt.
pub(crate) fn describe_directive(d: &Directive) -> String {
    use umf_core::ast::RunCommand;
    let raw = match d {
        Directive::Run(r) => {
            let cmd_text = match &r.command {
                RunCommand::Shell(s) => s.value.clone(),
                RunCommand::Exec(parts) => parts
                    .iter()
                    .map(|p| p.value.clone())
                    .collect::<Vec<_>>()
                    .join(" "),
            };
            format!("RUN {cmd_text}")
        }
        Directive::Add(a) => {
            let src = a.source.as_str();
            format!("ADD {src} {dst}", dst = a.destination.value.as_str())
        }
        Directive::Label(_) => "LABEL".to_string(),
        Directive::Env(_) => "ENV".to_string(),
        Directive::Arg(_) => "ARG".to_string(),
        Directive::User(_) => "USER".to_string(),
        Directive::Workdir(_) => "WORKDIR".to_string(),
        Directive::Entrypoint(_) => "ENTRYPOINT".to_string(),
        Directive::Expose(_) => "EXPOSE".to_string(),
        Directive::Shell(_) => "SHELL".to_string(),
        Directive::Cmd(_) => "CMD".to_string(),
        Directive::Volume(_) => "VOLUME".to_string(),
        Directive::Stopsignal(_) => "STOPSIGNAL".to_string(),
    };
    truncate_for_prompt(&raw, 80)
}

fn truncate_for_prompt(s: &str, max: usize) -> String {
    let oneline = s.replace('\n', " ");
    if oneline.chars().count() <= max {
        oneline
    } else {
        // Cut on a char boundary: the old `&oneline[..max-1]` byte-slice
        // panicked when a RUN command had a multi-byte char at the boundary.
        let head: String = oneline.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// Outcome of applying one directive.
pub(crate) enum StepFlow {
    /// Directive applied; proceed to the next step.
    Continue,
    /// A `RUN` missed the cache *after* an earlier step in the stage was
    /// adopted from cache, so the overlay lower stack is missing layers and
    /// executing here would build against an incomplete filesystem. The caller
    /// must discard this attempt and rebuild the stage with cache lookups
    /// disabled so every step re-executes and `upper_guards` is complete.
    RebuildWithoutCache,
}

/// Read-only context threaded through every directive handler in a stage build:
/// the RUN runtime, the OCI layout, the step cache, resolved build secrets, the
/// build-context dir, and the multi-stage `produced` map. Built once per stage
/// in `build_one_stage`, it replaces the six-ref argument clump that used to
/// ride through `apply_directive` / `run::apply_run` / `add::apply_add*`.
pub(crate) struct BuildCtx<'a> {
    /// libcontainer runtime that executes RUN steps.
    pub runtime: &'a LibcontainerRuntime,
    /// On-disk OCI layout (base pulls, stage reads, cache sidecars).
    pub layout: &'a ImageLayout,
    /// Per-step content-addressed layer cache.
    pub cache: &'a StepCache,
    /// Materialised build-time secrets, looked up by RUN `--mount`.
    pub secrets: &'a ResolvedSecrets,
    /// Build context directory (source root for `ADD`).
    pub context_dir: &'a Path,
    /// Layer compression codec for this build (from
    /// [`super::EngineBuildOptions::compression`]); stamped onto the
    /// [`BuildState`] so packaging and cache keys agree on it.
    pub compression: LayerCompression,
    /// `ADD <url>` payloads, fetched once in [`super::build`]'s async
    /// phase and staged in tempfiles, keyed by the recipe's URL string.
    pub fetched_urls: &'a BTreeMap<String, FetchedUrl>,
    /// Map of completed stage name → in-layout ref (for `ADD --from`).
    pub produced: &'a BTreeMap<String, String>,
    /// Target CPU architecture (from `--platform`). Folded into the RUN /
    /// ADD cache keys so a layout shared across arches stays isolated.
    pub architecture: umf_core::architecture::Architecture,
    /// Build-global `ARG` scope (pre-`FROM` ARGs resolved against
    /// `--build-arg`). Seeds each stage's [`BuildState::arg_scope`] and
    /// substitutes the `FROM` reference. See [`super::resolve_global_args`].
    pub globals: &'a BTreeMap<String, String>,
    /// Raw `--build-arg NAME=VALUE` overrides, applied to an in-stage `ARG`
    /// when one is re-declared (Docker semantics).
    pub build_args: &'a BTreeMap<String, String>,
}

pub(crate) fn apply_directive(
    ctx: &BuildCtx,
    state: &mut BuildState,
    bundle: &mut Bundle,
    directive: &Directive,
    step_n: u32,
    lookup_cache: bool,
) -> Result<StepFlow, EngineBuildError> {
    match directive {
        Directive::Run(run) => run::apply_run(ctx, state, bundle, run, step_n, lookup_cache),
        Directive::Add(add) => {
            add::apply_add(ctx, state, add, lookup_cache).map(|()| StepFlow::Continue)
        }
        Directive::Workdir(wd) => {
            apply_workdir(state, wd)?;
            Ok(StepFlow::Continue)
        }
        Directive::User(u) => {
            apply_user(state, u);
            Ok(StepFlow::Continue)
        }
        Directive::Env(e) => {
            apply_env(state, e);
            Ok(StepFlow::Continue)
        }
        Directive::Arg(arg) => {
            apply_arg(ctx, state, arg);
            Ok(StepFlow::Continue)
        }
        Directive::Shell(s) => {
            apply_shell(state, s);
            Ok(StepFlow::Continue)
        }
        Directive::Label(l) => {
            apply_label(state, l);
            Ok(StepFlow::Continue)
        }
        Directive::Entrypoint(ep) => apply_entrypoint(state, &ep.init).map(|()| StepFlow::Continue),
        Directive::Expose(e) => {
            apply_expose(state, e);
            Ok(StepFlow::Continue)
        }
        Directive::Cmd(c) => {
            apply_cmd(state, c);
            Ok(StepFlow::Continue)
        }
        Directive::Volume(v) => {
            apply_volume(state, v);
            Ok(StepFlow::Continue)
        }
        Directive::Stopsignal(s) => {
            apply_stopsignal(state, s);
            Ok(StepFlow::Continue)
        }
    }
}

/// Record a freshly-pushed `layer` in the step cache under `key`. The RUN, ADD,
/// and `ADD --from` handlers all finish with this identical store, so it lives
/// here, shared by [`super::run`] and [`super::add`].
pub(super) fn store_layer_cache(
    cache: &StepCache,
    key: &str,
    layer: &LayerSource,
    history_line: String,
) -> Result<(), EngineBuildError> {
    cache.store(
        key,
        &CachedStep {
            blob_digest: sha256_digest(&layer.data),
            diff_id: layer.diff_id.clone(),
            media_type: layer.media_type.clone(),
            history_line,
        },
    )?;
    Ok(())
}

/// `WORKDIR` (Docker-faithful): resolve the path (a relative path is joined onto
/// the current working dir), create the directory if missing — as its own layer,
/// so a later RUN can `chdir` into it and the directory ships in the image — and
/// set it as the working dir for subsequent steps.
///
/// The substituted, resolved path drives the directory + the image config; the
/// history line keeps the original recipe text, so no ARG value leaks into the
/// image.
fn apply_workdir(state: &mut BuildState, wd: &Workdir) -> Result<(), EngineBuildError> {
    let original = wd.path.value.as_str();
    let requested = state.subst(original);
    let current = state
        .image_config
        .container
        .working_dir
        .clone()
        .unwrap_or_else(|| "/".to_string());
    let resolved = crate::fsutil::resolve_workdir(&current, &requested);

    // Synthesise a one-directory upper-dir (mkdir -p) and push it as a layer.
    // `push_new_layer` records it in `upper_guards`, so the next RUN's overlay
    // lower stack includes it and the container's chdir into the working dir
    // succeeds (Docker auto-creates a missing WORKDIR; umf matches that here).
    let upper_holder = TempDir::new()?;
    let upper_root = upper_holder.path().join("upper");
    let dir = upper_root.join(resolved.trim_start_matches('/'));
    std::fs::create_dir_all(&dir)?;
    let persisted = PersistedUpper::from_owned_tempdir(upper_holder, upper_root);
    state.push_new_layer(persisted, format!("WORKDIR {original}"))?;

    state.image_config.container.working_dir = Some(resolved);
    Ok(())
}

fn apply_user(state: &mut BuildState, u: &User) {
    let original = u.name.value.as_str();
    state.push_metadata_history(format!("USER {original}"));
    state.image_config.container.user = Some(state.subst(original));
}

fn apply_env(state: &mut BuildState, e: &Env) {
    let key = e.key.value.as_str().to_string();
    let original_value = e.value.value.as_str();
    // The substituted value lands in the image config (an explicit `ENV
    // X=${VAR}` is the author asking for it to persist — matching Docker); the
    // history line keeps the original `${VAR}` text.
    let value = state.subst(original_value);
    let entry = format!("{key}={value}");
    // Replace any existing `KEY=...` entry rather than appending a
    // duplicate — later ENV wins on the same key.
    let prefix = format!("{key}=");
    state
        .image_config
        .container
        .env
        .retain(|e| !e.starts_with(&prefix));
    state.image_config.container.env.push(entry);
    state.push_metadata_history(format!("ENV {key}={original_value}"));
}

fn apply_shell(state: &mut BuildState, s: &Shell) {
    // The argv is already resolved by the parser (keyword forms expanded, exec
    // form verbatim); an empty argv is `SHELL none`. The shell-form RUN / CMD /
    // ENTRYPOINT handlers fall back to `/bin/sh -c` when it is empty.
    let argv: Vec<String> = s.argv.iter().map(|a| a.value.clone()).collect();
    state.push_metadata_history(format!(
        "SHELL {}",
        if argv.is_empty() {
            "(none)"
        } else {
            "switched"
        }
    ));
    state.current_shell = argv;
}

fn apply_label(state: &mut BuildState, l: &Label) {
    let key = l.key.value.as_str().to_string();
    let original_value = l.value.value.as_str();
    state.push_metadata_history(format!("LABEL {key}={original_value}"));
    state
        .image_config
        .container
        .labels
        .insert(key, state.subst(original_value));
}

/// Apply an in-stage `ARG`: update the substitution scope so the
/// directives that follow in this stage see the value. Precedence: a
/// `--build-arg` override, then the declared default, then any value already in
/// scope (e.g. a build-global of the same name, re-declared without a default).
/// An `ARG` with none of these leaves the name unset, so references to it stay
/// verbatim.
///
/// `ARG` emits no layer and no history line: its only effect is on substitution
/// and, transitively, on the RUN / ADD cache keys, which fold in the
/// substituted operands — so a changed value rebuilds correctly.
fn apply_arg(ctx: &BuildCtx, state: &mut BuildState, arg: &Arg) {
    crate::arg_subst::apply_arg_to_scope(&mut state.arg_scope, arg, ctx.build_args);
}

fn apply_entrypoint(state: &mut BuildState, init: &EntrypointInit) -> Result<(), EngineBuildError> {
    match init {
        EntrypointInit::Path(spanned) => {
            // Shell-form ENTRYPOINT slurps argv into a single command;
            // we run it via the current shell at image runtime. `${VAR}` is
            // substituted for the stored command; history keeps the original.
            let original = spanned.value.as_str();
            let mut argv = state.current_shell.clone();
            if argv.is_empty() {
                argv.push("/bin/sh".to_string());
                argv.push("-c".to_string());
            }
            argv.push(state.subst(original));
            state.image_config.container.entrypoint = Some(argv);
            state.push_metadata_history(format!("ENTRYPOINT {original}"));
            Ok(())
        }
        EntrypointInit::Exec(parts) => {
            let argv: Vec<String> = parts
                .iter()
                .map(|p| state.subst(p.value.as_str()))
                .collect();
            state.push_metadata_history(format!(
                "ENTRYPOINT {}",
                parts
                    .iter()
                    .map(|p| p.value.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
            state.image_config.container.entrypoint = Some(argv);
            Ok(())
        }
        EntrypointInit::None => {
            state.push_metadata_history("ENTRYPOINT none".to_string());
            state.image_config.container.entrypoint = None;
            Ok(())
        }
        EntrypointInit::Systemd => {
            // systemd as PID 1: canonical invocation is `/usr/sbin/init`
            // on modern distros (and `/sbin/init` for legacy paths via
            // symlinks). Engine path sets the entrypoint accordingly so
            // the resulting image runs systemd when launched.
            state.image_config.container.entrypoint = Some(vec!["/usr/sbin/init".to_string()]);
            state.push_metadata_history("ENTRYPOINT systemd".to_string());
            Ok(())
        }
        EntrypointInit::OpenRc => {
            state.image_config.container.entrypoint = Some(vec!["/sbin/openrc-init".to_string()]);
            state.push_metadata_history("ENTRYPOINT openrc".to_string());
            Ok(())
        }
    }
}

// ── UMF-native semantics ────────────────────────────────────────────────────

/// EXPOSE → image-config metadata. UMF's spec calls for default-deny
/// nftables emission, but on the container target we follow the user's
/// directive in spirit while staying within OCI image-config: the
/// `exposed_ports` field is what container runtimes (podman, containerd)
/// surface to operators.
///
/// nftables emission lives in the bootable pipeline (where there's a host
/// network stack to firewall), not in the container target's image-config.
fn apply_expose(state: &mut BuildState, e: &Expose) {
    let proto = match e.protocol {
        ExposeProtocol::Tcp => "tcp",
        ExposeProtocol::Udp => "udp",
    };
    let entry = format!("{}/{proto}", e.port);
    if !state.image_config.container.exposed_ports.contains(&entry) {
        state
            .image_config
            .container
            .exposed_ports
            .push(entry.clone());
    }
    state.push_metadata_history(format!("EXPOSE {entry}"));
}

/// CMD → image-config `Cmd`. Shell form is wrapped by the build's shell (as
/// shell-form ENTRYPOINT is); exec form is verbatim argv. Last CMD wins.
fn apply_cmd(state: &mut BuildState, c: &Cmd) {
    // `${VAR}` is substituted for the stored argv; history keeps the original.
    let (argv, history) = match &c.command {
        CmdForm::Shell(s) => {
            let original = s.value.as_str();
            let mut argv = state.current_shell.clone();
            if argv.is_empty() {
                argv.push("/bin/sh".to_string());
                argv.push("-c".to_string());
            }
            argv.push(state.subst(original));
            (argv, original.to_string())
        }
        CmdForm::Exec(parts) => {
            let argv = parts
                .iter()
                .map(|p| state.subst(p.value.as_str()))
                .collect::<Vec<_>>();
            let history = parts
                .iter()
                .map(|p| p.value.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            (argv, history)
        }
    };
    state.push_metadata_history(format!("CMD {history}"));
    state.image_config.container.cmd = Some(argv);
}

/// VOLUME → image-config `Volumes` (advisory mount-point metadata, deduplicated).
fn apply_volume(state: &mut BuildState, v: &Volume) {
    for path in &v.paths {
        let p = state.subst(path.value.as_str());
        if !state.image_config.container.volumes.contains(&p) {
            state.image_config.container.volumes.push(p);
        }
    }
    let joined = v
        .paths
        .iter()
        .map(|p| p.value.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    state.push_metadata_history(format!("VOLUME {joined}"));
}

/// STOPSIGNAL → image-config `StopSignal`. Last STOPSIGNAL wins.
fn apply_stopsignal(state: &mut BuildState, s: &Stopsignal) {
    let sig = s.signal.value.clone();
    state.push_metadata_history(format!("STOPSIGNAL {sig}"));
    state.image_config.container.stop_signal = Some(sig);
}

#[cfg(test)]
mod tests;
