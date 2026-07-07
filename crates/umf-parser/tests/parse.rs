//! End-to-end parser tests: source string → [`umf_core::ast::Ast`].
//!
//! These exercise the full lexer + grammar pipeline through
//! [`umf_parser::parse`], with the documented examples as fixtures.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use umf_core::ast::{AddSource, CmdForm, Directive, EntrypointInit, ExposeProtocol, FromSource};
use umf_parser::diagnostics::Severity;
use umf_parser::{parse, parse_with_warnings};

#[test]
fn minimal_scratch_stage() {
    let src = "FROM scratch\n";
    let ast = parse(src).expect("parse should succeed");
    assert_eq!(ast.stages.len(), 1);
    assert!(matches!(ast.stages[0].from.source, FromSource::Scratch));
    assert!(ast.stages[0].name.is_none());
    assert!(ast.stages[0].directives.is_empty());
}

#[test]
fn from_image_reference() {
    let src = "FROM debian:bookworm\n";
    let ast = parse(src).expect("parse should succeed");
    match &ast.stages[0].from.source {
        FromSource::Reference(r) => assert_eq!(r.value.as_str(), "debian:bookworm"),
        FromSource::Scratch => panic!("expected reference"),
    }
}

#[test]
fn from_with_as_stage_name() {
    let src = "FROM debian:bookworm AS builder\n";
    let ast = parse(src).expect("parse should succeed");
    let name = ast.stages[0].name.as_ref().expect("AS should be parsed");
    assert_eq!(name.value.as_str(), "builder");
}

#[test]
fn simple_container_build() {
    let src = "\
FROM alpine:3.21
ENTRYPOINT none
RUN apk add --no-cache nginx
ADD nginx.conf /etc/nginx/nginx.conf
EXPOSE 80/tcp
";
    let ast = parse(src).expect("parse should succeed");
    assert_eq!(ast.stages.len(), 1);
    assert_eq!(ast.stages[0].directives.len(), 4);
}

#[test]
fn full_bootable_build_with_all_directive_groups() {
    let src = "\
FROM imagilux/kernel-linux:7.0
LABEL org.imagilux.umf.flavor=systemd-boot
ADD myorg-base:1.0 /
ENTRYPOINT systemd

LABEL org.imagilux.umf.author=test
LABEL org.imagilux.umf.name=webserver

RUN apt-get update && apt-get install -y nginx
ADD nginx.conf /etc/nginx/nginx.conf
EXPOSE 80/tcp
EXPOSE 443/tcp
RUN systemctl enable nginx.service
";
    let ast = parse(src).expect("parse should succeed");
    assert_eq!(ast.stages.len(), 1);
    let stage = &ast.stages[0];

    let mut rootfs_add_seen = false;
    let mut flavor_label_seen = false;
    let mut entrypoint_seen = false;
    let mut expose_tcp_count = 0;
    for d in &stage.directives {
        match d {
            Directive::Add(a) => {
                if let AddSource::Oci(r) = &a.source {
                    assert_eq!(r.value.as_str(), "myorg-base:1.0");
                    rootfs_add_seen = true;
                }
            }
            Directive::Label(l) if l.key.value.as_str() == "org.imagilux.umf.flavor" => {
                assert_eq!(l.value.value.as_str(), "systemd-boot");
                flavor_label_seen = true;
            }
            Directive::Entrypoint(e) => {
                assert!(matches!(e.init, EntrypointInit::Systemd));
                entrypoint_seen = true;
            }
            Directive::Expose(e) if e.protocol == ExposeProtocol::Tcp => {
                expose_tcp_count += 1;
            }
            _ => {}
        }
    }
    assert!(rootfs_add_seen, "ADD <oci-ref> rootfs should be parsed");
    assert!(flavor_label_seen, "flavor LABEL should be parsed");
    assert!(entrypoint_seen, "ENTRYPOINT should be parsed");
    assert_eq!(expose_tcp_count, 2, "two EXPOSE …/tcp expected");
    // FROM is the kernel reference under the new spec.
    match &stage.from.source {
        FromSource::Reference(r) => assert_eq!(r.value.as_str(), "imagilux/kernel-linux:7.0"),
        FromSource::Scratch => panic!("expected FROM reference, not scratch"),
    }
}

#[test]
fn appliance_pattern_shell_form() {
    let src = "\
FROM imagilux/kernel-linux:7.0
LABEL org.imagilux.umf.flavor=uki
ENTRYPOINT /myapp

ADD myapp /myapp
";
    let ast = parse(src).expect("parse should succeed");
    let init = ast.stages[0]
        .directives
        .iter()
        .find_map(|d| match d {
            Directive::Entrypoint(e) => Some(&e.init),
            _ => None,
        })
        .expect("ENTRYPOINT present");
    match init {
        EntrypointInit::Path(s) => assert_eq!(s.value, "/myapp"),
        other => panic!("expected Path entrypoint, got {other:?}"),
    }
}

#[test]
fn entrypoint_exec_form_argv() {
    let src = "\
FROM alpine:3.21
ENTRYPOINT [\"/usr/sbin/nginx\", \"-g\", \"daemon off;\"]
";
    let ast = parse(src).expect("parse should succeed");
    let init = ast.stages[0]
        .directives
        .iter()
        .find_map(|d| match d {
            Directive::Entrypoint(e) => Some(&e.init),
            _ => None,
        })
        .expect("ENTRYPOINT present");
    match init {
        EntrypointInit::Exec(argv) => {
            let argv_values: Vec<&str> = argv.iter().map(|a| a.value.as_str()).collect();
            assert_eq!(argv_values, vec!["/usr/sbin/nginx", "-g", "daemon off;"]);
        }
        other => panic!("expected Exec entrypoint, got {other:?}"),
    }
}

#[test]
fn entrypoint_path_with_args_preserves_command_string() {
    // Shell-form ENTRYPOINT slurps the whole rest-of-line as a single command,
    // like RUN — args after the binary path stay in one preserved string.
    let src = "FROM alpine:3.21\nENTRYPOINT /usr/sbin/nginx --some-flag value\n";
    let ast = parse(src).expect("parse should succeed");
    let init = ast.stages[0]
        .directives
        .iter()
        .find_map(|d| match d {
            Directive::Entrypoint(e) => Some(&e.init),
            _ => None,
        })
        .expect("ENTRYPOINT present");
    match init {
        EntrypointInit::Path(s) => assert_eq!(s.value, "/usr/sbin/nginx --some-flag value"),
        other => panic!("expected Path entrypoint, got {other:?}"),
    }
}

#[test]
fn entrypoint_keyword_binary_is_rejected() {
    // `binary` is no longer a keyword — only systemd/openrc/none keywords are
    // valid, with leading-`/` paths for direct binary execution.
    let src = "FROM scratch\nENTRYPOINT binary\n";
    let result = parse(src);
    assert!(result.is_err(), "bare `binary` keyword must error");
}

#[test]
fn entrypoint_exec_empty_is_rejected() {
    let src = "FROM alpine:3.21\nENTRYPOINT []\n";
    let result = parse(src);
    assert!(result.is_err(), "empty exec form must error");
}

#[test]
fn multi_stage_with_named_stage() {
    let src = "\
FROM debian:bookworm AS builder
RUN make

FROM scratch
ADD --from=builder /app /app
";
    let ast = parse(src).expect("parse should succeed");
    assert_eq!(ast.stages.len(), 2);
    assert_eq!(
        ast.stages[0]
            .name
            .as_ref()
            .expect("first stage named")
            .value
            .as_str(),
        "builder"
    );
    assert!(ast.stages[1].name.is_none());

    // ADD --from=builder is captured on Add::from
    let add = ast.stages[1]
        .directives
        .iter()
        .find_map(|d| match d {
            Directive::Add(a) => Some(a),
            _ => None,
        })
        .expect("ADD present in second stage");
    let from = add.from.as_ref().expect("--from should be captured");
    assert_eq!(from.value.as_str(), "builder");
    assert_eq!(add.source.as_str(), "/app");
    assert_eq!(add.destination.value.as_str(), "/app");
}

#[test]
fn add_without_from_has_no_from() {
    let src = "FROM debian:bookworm\nADD ./src /work\n";
    let ast = parse(src).expect("parse should succeed");
    let add = ast.stages[0]
        .directives
        .iter()
        .find_map(|d| match d {
            Directive::Add(a) => Some(a),
            _ => None,
        })
        .expect("ADD present");
    assert!(add.from.is_none());
}

#[test]
fn add_oci_ref_source_is_oci() {
    // A ref-shaped bare source (a `:tag`) is an external OCI image — the rootfs
    // mechanism, replacing the removed ROOTFS directive. No `--from` needed.
    let src = "FROM imagilux/kernel-linux:7.0\nADD imagilux/rootfs:v7.0 /\n";
    let ast = parse(src).expect("parse should succeed");
    let add = ast.stages[0]
        .directives
        .iter()
        .find_map(|d| match d {
            Directive::Add(a) => Some(a),
            _ => None,
        })
        .expect("ADD present");
    match &add.source {
        AddSource::Oci(r) => assert_eq!(r.value.as_str(), "imagilux/rootfs:v7.0"),
        other => panic!("expected OCI source, got {other:?}"),
    }
    assert!(add.from.is_none(), "a bare OCI source carries no --from");
}

#[test]
fn add_source_scheme_and_path_discrimination() {
    // `oci://` (and `https+oci://`) force an OCI source; a `./`-prefixed source
    // is a local path even though the OCI heuristic would otherwise fire.
    let ast = parse(
        "FROM scratch\nADD oci://alpine:3.21 /\nADD ./local:weird /etc/x\nADD https://h/x.tar.gz /opt\n",
    )
    .expect("parse should succeed");
    let adds: Vec<_> = ast.stages[0]
        .directives
        .iter()
        .filter_map(|d| match d {
            Directive::Add(a) => Some(a),
            _ => None,
        })
        .collect();
    assert!(matches!(adds[0].source, AddSource::Oci(_)), "oci:// → Oci");
    assert!(
        matches!(adds[1].source, AddSource::Path(_)),
        "./ prefix → Path even with a colon",
    );
    assert!(
        matches!(adds[2].source, AddSource::Url(_)),
        "https:// → Url"
    );
}

#[test]
fn add_oci_ref_with_placeholder_parses_leniently() {
    // `ADD repo:${TAG} /` — a `${VAR}` in the tag is accepted now (lenient OCI
    // ref, like FROM) and resolved after build-time substitution.
    let src = "FROM imagilux/kernel-linux:7.0\nADD imagilux/rootfs:${TAG} /\n";
    let ast = parse(src).expect("placeholder OCI ADD must parse");
    let add = ast.stages[0]
        .directives
        .iter()
        .find_map(|d| match d {
            Directive::Add(a) => Some(a),
            _ => None,
        })
        .expect("ADD present");
    match &add.source {
        AddSource::Oci(r) => assert_eq!(r.value.as_str(), "imagilux/rootfs:${TAG}"),
        other => panic!("expected OCI source, got {other:?}"),
    }
}

#[test]
fn add_url_with_placeholder_parses_leniently() {
    // `ADD https://host/${VER}.tar /` — the placeholder rides through verbatim.
    let src = "FROM scratch\nADD https://host/${VER}.tar.gz /opt/\n";
    let ast = parse(src).expect("placeholder URL ADD must parse");
    let add = ast.stages[0]
        .directives
        .iter()
        .find_map(|d| match d {
            Directive::Add(a) => Some(a),
            _ => None,
        })
        .expect("ADD present");
    match &add.source {
        AddSource::Url(u) => assert_eq!(u.value.as_str(), "https://host/${VER}.tar.gz"),
        other => panic!("expected URL source, got {other:?}"),
    }
}

#[test]
fn add_from_without_value_is_an_error() {
    // `--from` alone (no value) should produce a diagnostic, not silently
    // succeed.
    let src = "FROM debian:bookworm\nADD --from /a /b\n";
    let result = parse(src);
    assert!(result.is_err(), "ADD --from without a value must error");
}

#[test]
fn run_mount_secret_is_captured() {
    let src = "\
FROM debian:bookworm
RUN --mount=type=secret,id=signing-key,target=/run/secrets/sk cat /run/secrets/sk
";
    let ast = parse(src).expect("parse should succeed");
    let run = ast.stages[0]
        .directives
        .iter()
        .find_map(|d| match d {
            Directive::Run(r) => Some(r),
            _ => None,
        })
        .expect("RUN present");
    assert_eq!(run.mounts.len(), 1, "secret mount captured");
    use umf_core::ast::RunMountKind;
    let RunMountKind::Secret { id, target } = &run.mounts[0].kind;
    assert_eq!(id.value.as_str(), "signing-key");
    assert_eq!(
        target.as_ref().map(|t| t.value.as_str()),
        Some("/run/secrets/sk")
    );
}

#[test]
fn run_mount_secret_target_optional() {
    let src = "FROM debian:bookworm\nRUN --mount=type=secret,id=k cat /run/secrets/k\n";
    let ast = parse(src).expect("parse should succeed");
    let run = ast.stages[0]
        .directives
        .iter()
        .find_map(|d| match d {
            Directive::Run(r) => Some(r),
            _ => None,
        })
        .expect("RUN present");
    use umf_core::ast::RunMountKind;
    let RunMountKind::Secret { id, target } = &run.mounts[0].kind;
    assert_eq!(id.value.as_str(), "k");
    assert!(target.is_none());
}

#[test]
fn run_mount_unsupported_type_errors() {
    let src = "FROM debian:bookworm\nRUN --mount=type=cache,id=apt apt update\n";
    let result = parse(src);
    assert!(result.is_err(), "unsupported mount type must error");
}

#[test]
fn run_mount_secret_without_id_errors() {
    let src = "FROM debian:bookworm\nRUN --mount=type=secret cat\n";
    let result = parse(src);
    assert!(result.is_err(), "secret mount without id must error");
}

#[test]
fn line_continuation_makes_one_directive() {
    let src = "FROM debian:bookworm\nRUN apt-get update && \\\n    apt-get install -y nginx\n";
    let ast = parse(src).expect("parse should succeed");
    assert_eq!(ast.stages[0].directives.len(), 1);
    let run = match &ast.stages[0].directives[0] {
        Directive::Run(r) => r,
        _ => panic!("expected RUN"),
    };
    match &run.command {
        umf_core::ast::RunCommand::Shell(s) => assert!(s.value.contains("apt-get install")),
        umf_core::ast::RunCommand::Exec(_) => panic!("expected shell form"),
    }
}

#[test]
fn comments_are_skipped() {
    let src = "\
# top-level comment
FROM scratch
# another comment between directives
ENTRYPOINT systemd
";
    let ast = parse(src).expect("parse should succeed");
    assert_eq!(ast.stages.len(), 1);
    assert_eq!(ast.stages[0].directives.len(), 1);
}

#[test]
fn unknown_directive_keyword_is_an_error() {
    let src = "FROM scratch\nFOOBAR something\n";
    let result = parse(src);
    assert!(result.is_err(), "FOOBAR should not lex as a Keyword");
    let diags = result.expect_err("expected errors");
    assert!(!diags.is_empty());
}

#[test]
fn missing_from_is_an_error() {
    let src = "RUN echo hi\n";
    let result = parse(src);
    assert!(result.is_err());
}

// ── Strongly-typed-argument negative cases ──────────────────────────────────
//
// These exercise the umf-core::types newtypes through the parser pipeline,
// confirming that each malformed value (a) produces a Diagnostic, and (b)
// the sub-span points at the offending byte. Sub-span verification is done
// against the absolute byte offset inside the source string.

fn first_diag(src: &str) -> umf_parser::diagnostics::Diagnostic {
    parse(src)
        .expect_err("expected diagnostics for malformed input")
        .into_iter()
        .next()
        .expect("at least one diagnostic")
}

#[test]
fn label_key_leading_digit_diagnostic_points_at_digit() {
    // The `2` in `2bad` is at byte offset 18 in the source.
    let src = "FROM scratch\nLABEL 2bad=foo\n";
    let diag = first_diag(src);
    assert!(diag.message.contains("LABEL"));
    let pos = src.find('2').unwrap();
    assert_eq!(diag.primary.span.start, pos);
    assert_eq!(diag.primary.span.end, pos + 1);
}

#[test]
fn label_key_trailing_dot_is_rejected() {
    let src = "FROM scratch\nLABEL foo.=bar\n";
    let diag = first_diag(src);
    assert!(diag.message.contains("LABEL"));
}

#[test]
fn env_var_name_leading_digit_is_rejected() {
    let src = "FROM scratch\nENV 2PATH=/x\n";
    let diag = first_diag(src);
    assert!(diag.message.contains("ENV"));
    let pos = src.find('2').unwrap();
    assert_eq!(diag.primary.span.start, pos);
}

#[test]
fn env_var_name_dash_is_rejected() {
    let src = "FROM scratch\nENV BAD-NAME=x\n";
    let diag = first_diag(src);
    assert!(diag.message.contains("ENV"));
}

#[test]
fn arg_name_leading_digit_is_rejected() {
    let src = "FROM scratch\nARG 1VERSION=1\n";
    let diag = first_diag(src);
    assert!(diag.message.contains("ARG"));
}

#[test]
fn stage_name_leading_digit_is_rejected() {
    let src = "FROM scratch AS 1stage\n";
    let diag = first_diag(src);
    assert!(diag.message.contains("AS"));
}

#[test]
fn add_from_invalid_stage_name_is_rejected() {
    let src = "FROM debian:bookworm AS builder\n\nFROM scratch\nADD --from=bad.name /a /b\n";
    let diag = first_diag(src);
    assert!(diag.message.contains("ADD"));
}

#[test]
fn user_invalid_leading_char_is_rejected() {
    let src = "FROM scratch\nUSER Nginx\n";
    let diag = first_diag(src);
    assert!(diag.message.contains("USER"));
}

#[test]
fn user_empty_group_is_rejected() {
    let src = "FROM scratch\nUSER nginx:\n";
    let diag = first_diag(src);
    assert!(diag.message.contains("USER"));
}

#[test]
fn enable_directive_is_removed() {
    // ENABLE/DISABLE are not UMF directives; enable units directly instead.
    let diag = first_diag("FROM scratch\nENABLE nginx.service\n");
    assert!(diag.message.contains("ENABLE"), "got: {}", diag.message);
    assert!(diag.message.contains("not a UMF directive"));
}

#[test]
fn disable_directive_is_removed() {
    let diag = first_diag("FROM scratch\nDISABLE nginx.service\n");
    assert!(diag.message.contains("DISABLE"), "got: {}", diag.message);
    assert!(diag.message.contains("not a UMF directive"));
}

#[test]
fn secret_id_invalid_char_is_rejected() {
    let src = "FROM debian:bookworm\nRUN --mount=type=secret,id=bad/id cat /dev/null\n";
    let diag = first_diag(src);
    assert!(
        diag.message.contains("secret") || diag.message.contains("id"),
        "got: {}",
        diag.message
    );
}

// ── Path / OCI ref / URL negative tests (PR2) ────────────────────────────────

#[test]
fn workdir_relative_path_is_accepted() {
    // Permissive: a relative WORKDIR is joined against the previous
    // WORKDIR (or `/` if none). The parser preserves the spelling verbatim;
    // the join is the builder's job.
    let src = "FROM scratch\nWORKDIR relative/path\n";
    let ast = parse(src).expect("relative WORKDIR should parse");
    let wd = ast.stages[0]
        .directives
        .iter()
        .find_map(|d| match d {
            Directive::Workdir(w) => Some(w),
            _ => None,
        })
        .expect("WORKDIR present");
    assert_eq!(wd.path.value.as_str(), "relative/path");
}

#[test]
fn workdir_double_slash_is_accepted() {
    // Permissive: `//` runs are tolerated and preserved verbatim.
    let src = "FROM scratch\nWORKDIR /etc//foo\n";
    let ast = parse(src).expect("`//` WORKDIR should parse");
    let wd = ast.stages[0]
        .directives
        .iter()
        .find_map(|d| match d {
            Directive::Workdir(w) => Some(w),
            _ => None,
        })
        .expect("WORKDIR present");
    assert_eq!(wd.path.value.as_str(), "/etc//foo");
}

#[test]
fn workdir_trailing_slash_is_accepted() {
    // Permissive: a trailing `/` is allowed and preserved verbatim.
    let src = "FROM scratch\nWORKDIR /etc/\n";
    let ast = parse(src).expect("trailing-slash WORKDIR should parse");
    let wd = ast.stages[0]
        .directives
        .iter()
        .find_map(|d| match d {
            Directive::Workdir(w) => Some(w),
            _ => None,
        })
        .expect("WORKDIR present");
    assert_eq!(wd.path.value.as_str(), "/etc/");
}

#[test]
fn add_destination_relative_is_accepted() {
    // Permissive: a relative destination is resolved against the
    // current WORKDIR. The parser preserves the spelling verbatim.
    let src = "FROM scratch\nADD ./foo bar/baz\n";
    let ast = parse(src).expect("relative ADD destination should parse");
    let add = ast.stages[0]
        .directives
        .iter()
        .find_map(|d| match d {
            Directive::Add(a) => Some(a),
            _ => None,
        })
        .expect("ADD present");
    assert_eq!(add.destination.value.as_str(), "bar/baz");
}

#[test]
fn add_destination_trailing_slash_is_preserved() {
    // A trailing `/` on ADD/COPY forces directory semantics. We preserve
    // the slash verbatim so that signal survives.
    let src = "FROM scratch\nADD https://example.com/x.tar.gz /opt/\n";
    let ast = parse(src).expect("trailing-slash ADD destination should parse");
    let add = ast.stages[0]
        .directives
        .iter()
        .find_map(|d| match d {
            Directive::Add(a) => Some(a),
            _ => None,
        })
        .expect("ADD present");
    assert_eq!(add.destination.value.as_str(), "/opt/");
    assert!(add.destination.value.has_trailing_slash());
}

#[test]
fn add_url_valid_is_accepted_as_url_variant() {
    let src = "FROM scratch\nADD https://example.com/file.tar.gz /opt/file.tar.gz\n";
    let ast = parse(src).expect("https URL should parse");
    let add = ast.stages[0]
        .directives
        .iter()
        .find_map(|d| match d {
            Directive::Add(a) => Some(a),
            _ => None,
        })
        .expect("ADD present");
    use umf_core::ast::AddSource;
    assert!(matches!(add.source, AddSource::Url(_)));
}

#[test]
fn add_local_path_is_path_variant() {
    let src = "FROM scratch\nADD ./local.txt /opt/local.txt\n";
    let ast = parse(src).expect("local path should parse");
    let add = ast.stages[0]
        .directives
        .iter()
        .find_map(|d| match d {
            Directive::Add(a) => Some(a),
            _ => None,
        })
        .expect("ADD present");
    use umf_core::ast::AddSource;
    assert!(matches!(add.source, AddSource::Path(_)));
}

#[test]
fn from_uppercase_repository_is_rejected() {
    let src = "FROM Alpine:3.21\n";
    let diag = first_diag(src);
    assert!(diag.message.contains("FROM"));
}

#[test]
fn from_empty_tag_is_rejected() {
    let src = "FROM alpine:\n";
    let diag = first_diag(src);
    assert!(diag.message.contains("FROM"));
}

#[test]
fn from_with_host_and_port_is_accepted() {
    let src = "FROM quay.io:5000/imagilux/k:7.0\n";
    let ast = parse(src).expect("registry/host:port/path:tag should parse");
    if let FromSource::Reference(r) = &ast.stages[0].from.source {
        // OciReference is a validated newtype — the host:port/path:tag form is
        // accepted and preserved verbatim (decomposition lives downstream).
        assert_eq!(r.value.as_str(), "quay.io:5000/imagilux/k:7.0");
    } else {
        panic!("expected Reference");
    }
}

#[test]
fn rootfs_directive_is_removed() {
    // `ROOTFS` is not a UMF directive; the parser emits a migration diagnostic
    // pointing at the `ADD --from=<ref> / /` replacement.
    let src = "FROM scratch\nROOTFS alpine:3.21\n";
    let diag = first_diag(src);
    assert!(diag.message.contains("ROOTFS"));
    assert!(diag.message.contains("not a UMF directive"));
}

#[test]
fn secret_target_relative_path_is_accepted() {
    // BuildKit accepts a relative target for `--mount=type=secret`,
    // resolving it against the build's WORKDIR. We accept the same.
    let src = "FROM debian:bookworm\nRUN --mount=type=secret,id=k,target=relative cat foo\n";
    let ast = parse(src).expect("relative secret target should parse");
    let run = ast.stages[0]
        .directives
        .iter()
        .find_map(|d| match d {
            Directive::Run(r) => Some(r),
            _ => None,
        })
        .expect("RUN present");
    use umf_core::ast::RunMountKind;
    let RunMountKind::Secret { target, .. } = &run.mounts[0].kind;
    let target = target.as_ref().expect("target present");
    assert_eq!(target.value.as_str(), "relative");
}

// ── HOSTNAME / LOCALE / TIMEZONE (rejected) ──────────────────────────────────
// Host/locale/timezone are first-boot concerns (cloud-init / ignition); the
// parser recognizes each keyword and emits a migration diagnostic.

#[test]
fn hostname_directive_is_removed() {
    let diag = first_diag("FROM scratch\nHOSTNAME webserver\n");
    assert!(diag.message.contains("HOSTNAME"), "got: {}", diag.message);
    assert!(diag.message.contains("not a UMF directive"));
}

#[test]
fn locale_directive_is_removed() {
    let diag = first_diag("FROM scratch\nLOCALE en_US.UTF-8\n");
    assert!(diag.message.contains("LOCALE"), "got: {}", diag.message);
    assert!(diag.message.contains("not a UMF directive"));
}

#[test]
fn timezone_directive_is_removed() {
    let diag = first_diag("FROM scratch\nTIMEZONE Europe/Paris\n");
    assert!(diag.message.contains("TIMEZONE"), "got: {}", diag.message);
    assert!(diag.message.contains("not a UMF directive"));
}

// ── CMD / VOLUME / STOPSIGNAL ────────────────────────────────────────────────

#[test]
fn cmd_shell_form_parses() {
    let ast = parse("FROM alpine:3\nCMD echo hello world\n").expect("parse");
    let Some(Directive::Cmd(c)) = ast.stages[0].directives.last() else {
        panic!("expected a CMD directive");
    };
    let CmdForm::Shell(s) = &c.command else {
        panic!("expected shell form");
    };
    assert_eq!(s.value, "echo hello world");
}

#[test]
fn cmd_exec_form_parses() {
    let ast = parse("FROM alpine:3\nCMD [\"/bin/app\", \"--flag\"]\n").expect("parse");
    let Some(Directive::Cmd(c)) = ast.stages[0].directives.last() else {
        panic!("expected a CMD directive");
    };
    let CmdForm::Exec(argv) = &c.command else {
        panic!("expected exec form");
    };
    let got: Vec<&str> = argv.iter().map(|s| s.value.as_str()).collect();
    assert_eq!(got, ["/bin/app", "--flag"]);
}

#[test]
fn volume_space_separated_paths() {
    let ast = parse("FROM alpine:3\nVOLUME /data /var/log\n").expect("parse");
    let Some(Directive::Volume(v)) = ast.stages[0].directives.last() else {
        panic!("expected a VOLUME directive");
    };
    let got: Vec<&str> = v.paths.iter().map(|p| p.value.as_str()).collect();
    assert_eq!(got, ["/data", "/var/log"]);
}

#[test]
fn volume_array_form() {
    let ast = parse("FROM alpine:3\nVOLUME [\"/data\", \"/cache\"]\n").expect("parse");
    let Some(Directive::Volume(v)) = ast.stages[0].directives.last() else {
        panic!("expected a VOLUME directive");
    };
    assert_eq!(v.paths.len(), 2);
    assert_eq!(v.paths[1].value, "/cache");
}

#[test]
fn stopsignal_name_and_number() {
    for (src, want) in [
        ("FROM alpine:3\nSTOPSIGNAL SIGTERM\n", "SIGTERM"),
        ("FROM alpine:3\nSTOPSIGNAL 15\n", "15"),
    ] {
        let ast = parse(src).expect("parse");
        let Some(Directive::Stopsignal(s)) = ast.stages[0].directives.last() else {
            panic!("expected a STOPSIGNAL directive");
        };
        assert_eq!(s.signal.value, want);
    }
}

#[test]
fn bootloader_directive_is_removed() {
    // `BOOTLOADER` is not a UMF directive; packaging is now a flavor LABEL, so
    // the parser emits a migration diagnostic.
    let src = "FROM imagilux/kernel-linux:7.0\nBOOTLOADER none\nENTRYPOINT /app\n";
    let diag = first_diag(src);
    assert!(diag.message.contains("BOOTLOADER"));
    assert!(diag.message.contains("not a UMF directive"));
}

#[test]
fn multibyte_utf8_in_failing_directive_does_not_panic() {
    // Regression: a non-ASCII byte inside a word that fails newtype validation
    // makes the validator report a *byte* offset that can land inside a
    // multi-byte UTF-8 sequence. Diagnostic construction must snap to a char
    // boundary instead of panicking on a non-boundary `&str` slice — a parser
    // must never crash on malformed input. `FROM café` is the original
    // reproducer (the offset lands inside 'é').
    assert!(
        parse("FROM café\n").is_err(),
        "non-ASCII OCI reference should yield diagnostics, not panic"
    );

    // Other directives carrying non-ASCII bytes — none may panic, regardless of
    // whether they parse or error.
    for src in [
        "FROM scratch\nHOSTNAME nö\n",
        "FROM scratch\nLABEL clé=value\n",
        "FROM scratch\nENV clé=value\n",
        "RUN café\n",
    ] {
        let _ = parse(src);
    }
}

// ── ARG: parse, placement, and the secret-name nudge ────────────
//
// ARG is a real directive: it parses, stays in the AST, and the builder
// performs `${VAR}` substitution at build time (so the parser no longer warns
// about un-substituted references). A pre-FROM ARG is build-global
// (`Ast::global_args`); a non-ARG directive before FROM is a precise error. A
// secret-shaped ARG name gets a warning nudge, never a refusal. (MAINTAINER and
// other non-OCI Docker directives still warn-and-skip.)

#[test]
fn maintainer_warns_and_is_skipped_not_a_parse_error() {
    // `MAINTAINER` is a non-OCI Docker directive: recognized, warned, and
    // skipped (use `LABEL org.opencontainers.image.authors`). CMD/VOLUME/
    // STOPSIGNAL are now real directives, so they are no longer in
    // this warn-and-skip set.
    let src = "FROM scratch\nMAINTAINER me@example.com\n";
    let (ast, warnings) =
        parse_with_warnings(src).expect("MAINTAINER must warn-and-skip, not fail the parse");
    assert!(ast.stages[0].directives.is_empty());
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].severity, Severity::Warning);
    assert!(
        warnings[0].message.contains("MAINTAINER") && warnings[0].message.contains("not supported"),
        "got: {}",
        warnings[0].message
    );
}

#[test]
fn copy_parses_as_a_plain_add() {
    // COPY is a real directive: it shares the ADD node but
    // carries `plain_copy`, which the builder uses to reject remote sources.
    let src = "FROM scratch\nCOPY app /app\n";
    let (ast, warnings) = parse_with_warnings(src).expect("COPY must parse");
    assert!(warnings.is_empty(), "COPY must not warn: {warnings:?}");
    assert_eq!(ast.stages[0].directives.len(), 1);
    let Directive::Add(add) = &ast.stages[0].directives[0] else {
        panic!("COPY should parse to Directive::Add");
    };
    assert!(add.plain_copy, "COPY must set plain_copy");
    assert!(
        matches!(&add.source, AddSource::Path(p) if p.value.as_str() == "app"),
        "got: {:?}",
        add.source
    );
    assert_eq!(add.destination.value.as_str(), "/app");
    assert!(add.from.is_none());
}

#[test]
fn copy_supports_from_stage_and_add_stays_plain_copy_false() {
    // `COPY --from=<stage>` routes through the same parser; ADD must keep
    // `plain_copy == false` so the two verbs stay distinguishable downstream.
    let src = "FROM scratch AS build\nFROM scratch\nCOPY --from=build /a /b\nADD ./c /d\n";
    let ast = parse(src).expect("COPY --from + ADD must parse");
    let dirs = &ast.stages[1].directives;
    let Directive::Add(copy) = &dirs[0] else {
        panic!("expected COPY → Add");
    };
    assert!(copy.plain_copy);
    assert_eq!(copy.from.as_ref().map(|f| f.value.as_str()), Some("build"));
    let Directive::Add(add) = &dirs[1] else {
        panic!("expected ADD → Add");
    };
    assert!(!add.plain_copy, "ADD must leave plain_copy false");
}

#[test]
fn unknown_directive_is_still_a_hard_error() {
    // Only *known* Dockerfile directives warn-and-skip; a genuine typo must
    // still error so it isn't silently ignored.
    let src = "FROM scratch\nFROBNICATE x\n";
    assert!(
        parse_with_warnings(src).is_err(),
        "an unknown directive must remain a parse error"
    );
}

#[test]
fn arg_after_from_parses_without_warning() {
    // ARG is supported now: it parses, stays in the AST, and does not warn
    // (substitution is the builder's job at build time).
    let src = "FROM scratch\nARG BUILD_GID=1000\n";
    let (ast, warnings) = parse_with_warnings(src).expect("ARG must parse");
    assert_eq!(ast.stages[0].directives.len(), 1);
    let Directive::Arg(arg) = &ast.stages[0].directives[0] else {
        panic!("expected Directive::Arg");
    };
    assert_eq!(arg.name.value.as_str(), "BUILD_GID");
    assert!(
        warnings.is_empty(),
        "a normal ARG must not warn: {warnings:?}"
    );
}

#[test]
fn arg_before_from_is_captured_as_a_global_arg() {
    // ARG is the only directive allowed before FROM; it lands in global_args
    // and may be referenced in the FROM line (substituted at build time).
    let src = "ARG VERSION=stable\nFROM myapp:${VERSION}\n";
    let ast = parse(src).expect("pre-FROM ARG must parse");
    assert_eq!(ast.global_args.len(), 1);
    assert_eq!(ast.global_args[0].name.value.as_str(), "VERSION");
    assert_eq!(
        ast.global_args[0]
            .default
            .as_ref()
            .map(|d| d.value.as_str()),
        Some("stable")
    );
    // The FROM ref carries a `${VERSION}` placeholder verbatim — it parses now
    // (lenient OCI ref) and is resolved after build-time substitution.
    assert_eq!(
        ast.stages.len(),
        1,
        "the FROM still opens exactly one stage"
    );
    let FromSource::Reference(r) = &ast.stages[0].from.source else {
        panic!("expected a FROM reference");
    };
    assert_eq!(r.value.as_str(), "myapp:${VERSION}");
}

#[test]
fn multiple_args_before_from_all_captured() {
    let src = "ARG A=1\nARG B=2\nFROM scratch\n";
    let ast = parse(src).expect("multiple pre-FROM ARGs must parse");
    let names: Vec<&str> = ast
        .global_args
        .iter()
        .map(|a| a.name.value.as_str())
        .collect();
    assert_eq!(names, ["A", "B"]);
    assert!(ast.stages[0].directives.is_empty());
}

#[test]
fn non_arg_directive_before_from_is_an_error() {
    // Only ARG may precede FROM. A LABEL (or anything else) before FROM is a
    // precise error, not a generic "expected FROM".
    let src = "LABEL foo=bar\nFROM scratch\n";
    let errs = parse_with_warnings(src).expect_err("LABEL before FROM must fail");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("only ARG may appear before FROM")),
        "got: {errs:?}"
    );
}

#[test]
fn secret_shaped_arg_name_warns_but_parses() {
    // A secret-shaped name (PASSWORD/TOKEN/SECRET/KEY) is a nudge toward
    // `RUN --mount=type=secret`, never a refusal.
    for name in ["DB_PASSWORD", "GITHUB_TOKEN", "API_KEY", "MY_SECRET"] {
        let src = format!("FROM scratch\nARG {name}\n");
        let (ast, warnings) = parse_with_warnings(&src).expect("secret-shaped ARG still parses");
        assert_eq!(ast.stages[0].directives.len(), 1);
        assert!(
            warnings.iter().any(|w| w.message.contains("secret-shaped")),
            "name {name} should warn; got: {warnings:?}"
        );
    }
}

#[test]
fn ordinary_arg_name_does_not_warn() {
    let src = "FROM scratch\nARG BUILD_VERSION=stable\n";
    let (_ast, warnings) = parse_with_warnings(src).expect("must parse");
    assert!(
        warnings.is_empty(),
        "ordinary ARG must not warn: {warnings:?}"
    );
}

#[test]
fn arg_and_env_accept_dotted_versioned_values() {
    // `1.0` lexes as `Number(1)` + `Ident(".0")`; the value read splices them.
    let src = "ARG VERSION=1.0\nFROM scratch\nENV SEMVER=1.2.3\nLABEL rev=4.5.6-rc1\n";
    let ast = parse(src).expect("dotted/versioned values must parse");
    assert_eq!(
        ast.global_args[0]
            .default
            .as_ref()
            .map(|d| d.value.as_str()),
        Some("1.0")
    );
    let envs: Vec<_> = ast.stages[0]
        .directives
        .iter()
        .filter_map(|d| match d {
            Directive::Env(e) => Some(e.value.value.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(envs, ["1.2.3"]);
    let labels: Vec<_> = ast.stages[0]
        .directives
        .iter()
        .filter_map(|d| match d {
            Directive::Label(l) => Some(l.value.value.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(labels, ["4.5.6-rc1"]);
}

#[test]
fn spec_arg_example_parses_verbatim() {
    // The exact example from the spec's ARG section must parse as written
    // (unquoted `1.0`, `${VERSION}` in FROM).
    let src = "ARG VERSION=1.0\nFROM myapp:${VERSION}\nRUN ./configure --release ${VERSION}\n";
    let ast = parse(src).expect("the spec's ARG example must parse");
    assert_eq!(
        ast.global_args[0]
            .default
            .as_ref()
            .map(|d| d.value.as_str()),
        Some("1.0")
    );
}

#[test]
fn expose_port_proto_still_splits_after_value_splice() {
    // Guard: the value-splice must not affect EXPOSE, which relies on `80` and
    // `/tcp` staying separate tokens.
    let src = "FROM scratch\nEXPOSE 80/tcp\n";
    let ast = parse(src).expect("EXPOSE must still parse");
    let exposed: Vec<_> = ast.stages[0]
        .directives
        .iter()
        .filter_map(|d| match d {
            Directive::Expose(e) => Some((e.port, e.protocol)),
            _ => None,
        })
        .collect();
    assert_eq!(exposed, [(80, ExposeProtocol::Tcp)]);
}

// ── Legacy / regular Dockerfile forms ────────────────────────────────────────

/// All directives in a stage, for the multi-item assertions below.
fn stage_directives(src: &str) -> Vec<Directive> {
    parse(src).expect("parse").stages[0].directives.clone()
}

#[test]
fn shell_exec_form_preserves_argv_verbatim() {
    // The Docker exec form keeps its argv exactly — including strict-mode bash.
    let ds =
        stage_directives("FROM scratch\nSHELL [\"/bin/bash\", \"-euo\", \"pipefail\", \"-c\"]\n");
    let argv = ds
        .iter()
        .find_map(|d| match d {
            Directive::Shell(s) => Some(s.argv.iter().map(|a| a.value.clone()).collect::<Vec<_>>()),
            _ => None,
        })
        .expect("SHELL present");
    assert_eq!(argv, ["/bin/bash", "-euo", "pipefail", "-c"]);
}

#[test]
fn shell_keyword_resolves_to_argv() {
    // The keyword shorthand resolves to its conventional argv; `none` is empty.
    let argv_of = |src: &str| {
        stage_directives(src)
            .iter()
            .find_map(|d| match d {
                Directive::Shell(s) => {
                    Some(s.argv.iter().map(|a| a.value.clone()).collect::<Vec<_>>())
                }
                _ => None,
            })
            .expect("SHELL present")
    };
    assert_eq!(argv_of("FROM scratch\nSHELL bash\n"), ["/bin/bash", "-c"]);
    assert_eq!(argv_of("FROM scratch\nSHELL sh\n"), ["/bin/sh", "-c"]);
    assert!(argv_of("FROM scratch\nSHELL none\n").is_empty());
}

#[test]
fn label_multi_pair_emits_one_directive_per_pair() {
    // `LABEL a=1 b=2 c=3` (regular Docker) yields one Label per pair, in order.
    let labels: Vec<(String, String)> = stage_directives("FROM scratch\nLABEL a=1 b=2 c=3\n")
        .iter()
        .filter_map(|d| match d {
            Directive::Label(l) => Some((l.key.value.to_string(), l.value.value.to_string())),
            _ => None,
        })
        .collect();
    assert_eq!(
        labels,
        [
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "2".to_string()),
            ("c".to_string(), "3".to_string()),
        ]
    );
}

#[test]
fn env_multi_pair_emits_one_directive_per_pair() {
    let envs: Vec<(String, String)> = stage_directives("FROM scratch\nENV A=1 B=2\n")
        .iter()
        .filter_map(|d| match d {
            Directive::Env(e) => Some((e.key.value.to_string(), e.value.value.to_string())),
            _ => None,
        })
        .collect();
    assert_eq!(
        envs,
        [
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "2".to_string()),
        ]
    );
}

#[test]
fn expose_defaults_to_tcp_without_protocol() {
    // Regular Docker `EXPOSE 8080` (no `/proto`) defaults to tcp.
    let ds = stage_directives("FROM scratch\nEXPOSE 8080\n");
    let exposed: Vec<_> = ds
        .iter()
        .filter_map(|d| match d {
            Directive::Expose(e) => Some((e.port, e.protocol)),
            _ => None,
        })
        .collect();
    assert_eq!(exposed, [(8080, ExposeProtocol::Tcp)]);
}

#[test]
fn expose_multiple_ports_mixed_protocols() {
    // `EXPOSE 80 443/udp 8080` — multiple ports, per-port optional protocol.
    let ds = stage_directives("FROM scratch\nEXPOSE 80 443/udp 8080\n");
    let exposed: Vec<_> = ds
        .iter()
        .filter_map(|d| match d {
            Directive::Expose(e) => Some((e.port, e.protocol)),
            _ => None,
        })
        .collect();
    assert_eq!(
        exposed,
        [
            (80, ExposeProtocol::Tcp),
            (443, ExposeProtocol::Udp),
            (8080, ExposeProtocol::Tcp),
        ]
    );
}

#[test]
fn parse_wrapper_succeeds_and_drops_warnings() {
    // The thin `parse` wrapper still succeeds on a warning-only recipe; it just
    // doesn't surface the warnings (for tests / internal callers that don't
    // render them).
    let src = "FROM scratch\nMAINTAINER me@example.com\n";
    let ast = parse(src).expect("warning-only recipe must parse via the wrapper");
    assert!(ast.stages[0].directives.is_empty());
}

#[test]
fn hostile_input_yields_a_bounded_diagnostic_set() {
    // A quarter-megabyte of invalid control bytes would, uncapped, collect one
    // fat diagnostic per byte (~256k diagnostics, gigabytes of RSS). The
    // collection cap must bound it to a small constant.
    use umf_parser::diagnostics::MAX_COLLECTED_DIAGNOSTICS;
    let hostile = "\u{1}".repeat(256 * 1024);
    let err = parse(&hostile).expect_err("all-control-byte input must fail");
    assert!(
        err.len() <= MAX_COLLECTED_DIAGNOSTICS + 4,
        "diagnostics must be bounded (got {}, cap {})",
        err.len(),
        MAX_COLLECTED_DIAGNOSTICS
    );
}
