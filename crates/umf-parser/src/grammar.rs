//! Token stream → [`Ast`].
//!
//! Hand-written recursive-descent parser. Consumes a `Vec<Token>` produced by
//! [`crate::lexer::tokenize`] and produces a [`Vec<Stage>`] wrapped in an
//! [`Ast`]. Per-directive parsing dispatches on the leading [`Keyword`].
//!
//! Error recovery is line-based: when a directive fails to parse, the parser
//! skips to the next newline and continues with the next directive, so a
//! single broken line yields one diagnostic rather than a cascade.
//!
//! Known limitations (planned follow-ups):
//!
//! - **RUN exec form** (`RUN ["a", "b"]`) is not yet handled — falls through
//!   to shell-form parsing, which will error out because the bracket isn't a
//!   valid command character.
//! - **`RUN --mount=…`** options are accepted by the lexer but currently the
//!   grammar drops them on the floor; the resulting [`Run`] has an empty
//!   `mounts` field even if mounts were declared.
//!
//! These produce a working parser for the documented examples but need
//! follow-up before secrets and exec-form `RUN` work end-to-end.

use umf_core::ast::{
    Add, AddSource, Arg, Ast, Cmd, CmdForm, Directive, Entrypoint, EntrypointInit, Env, Expose,
    ExposeProtocol, FromArg, FromSource, Label, Run, RunCommand, RunMount, RunMountKind, Shell,
    Span, Spanned, Stage, Stopsignal, User, Volume, Workdir,
};
use umf_core::types::{
    EnvVarName, EnvVarValue, HttpsUrl, LabelKey, LabelValue, OciReference, RecipePath, SecretId,
    StageName, Username, ValidationError,
};

use crate::diagnostics::{self, Annotation, Diagnostic};
use crate::lexer::{Keyword, Punct, Token, TokenKind};

/// Parse a token stream into an [`Ast`] plus any non-fatal warnings.
///
/// On any error returns the accumulated [`Diagnostic`]s. A successful parse
/// returns the [`Ast`] together with a (possibly empty) list of **warnings**:
/// recognized-but-unsupported Docker directives (`MAINTAINER`, `HEALTHCHECK`,
/// `ONBUILD`) that are skipped, and the secret-shaped-`ARG`-name nudge.
/// Warnings never fail the parse.
///
/// # Errors
/// Returns a non-empty `Vec<Diagnostic>` when one or more directives fail to
/// parse. Error recovery is per-line: each malformed directive contributes
/// (typically) one diagnostic.
pub fn parse(source: &str, tokens: Vec<Token>) -> Result<(Ast, Vec<Diagnostic>), Vec<Diagnostic>> {
    let mut parser = Parser::new(source, tokens);
    parser.parse_top();
    if !parser.errors.is_empty() {
        return Err(parser.errors);
    }
    let ast = Ast {
        global_args: parser.global_args,
        stages: parser.stages,
    };
    Ok((ast, parser.warnings))
}

struct Parser<'a> {
    source: &'a str,
    tokens: Vec<Token>,
    pos: usize,
    /// Build-global `ARG`s parsed from the pre-`FROM` preamble.
    global_args: Vec<Arg>,
    stages: Vec<Stage>,
    errors: Vec<Diagnostic>,
    /// Non-fatal diagnostics: recognized-but-unsupported directives and `${…}`
    /// references umf does not substitute. Surfaced on a successful parse;
    /// they never fail the build.
    warnings: Vec<Diagnostic>,
    /// Extra directives produced by a single source line that carries multiple
    /// items — `LABEL a=1 b=2`, `ENV a=1 b=2`, `EXPOSE 80 443` (regular Docker
    /// style). The line's parser returns the first item as the directive and
    /// queues the rest here; the stage loop drains the queue after each
    /// directive so they land in source order. Semantically identical to
    /// writing the items as separate directives.
    pending: Vec<Directive>,
}

impl<'a> Parser<'a> {
    fn new(source: &'a str, tokens: Vec<Token>) -> Self {
        Self {
            source,
            tokens,
            pos: 0,
            global_args: Vec::new(),
            stages: Vec::new(),
            errors: Vec::new(),
            warnings: Vec::new(),
            pending: Vec::new(),
        }
    }

    // ----- top-level / stage parsing -----

    fn parse_top(&mut self) {
        self.skip_newlines();
        self.parse_global_args();
        while self.pos < self.tokens.len() {
            match self.parse_stage() {
                Some(stage) => self.stages.push(stage),
                None => self.recover_to_next_stage(),
            }
            self.skip_newlines();
            // Bound both memory and time on a pathological input (e.g. millions
            // of tiny invalid stages): once the diagnostic set is capped, stop.
            if self.errors.len() > diagnostics::MAX_COLLECTED_DIAGNOSTICS {
                break;
            }
        }
    }

    /// Consume the pre-`FROM` preamble: `ARG` is the **only**
    /// directive permitted before the first `FROM`. A pre-`FROM` `ARG` is
    /// global to the build and may be referenced in a `FROM` line.
    ///
    /// Stops at the first `FROM` (handing off to the stage loop) or at any
    /// non-`ARG` directive, which is a clear error naming the offender — so a
    /// misplaced `LABEL`/`ENV`/… before `FROM` is diagnosed precisely rather
    /// than as a generic "expected FROM".
    fn parse_global_args(&mut self) {
        loop {
            self.skip_newlines();
            let Some(tok) = self.peek().cloned() else {
                return; // EOF — the stage loop will report the missing FROM.
            };
            match &tok.kind {
                TokenKind::Keyword(Keyword::Arg) => {
                    self.advance(); // consume ARG
                    match self.parse_arg(tok.span.start) {
                        Some(Directive::Arg(arg)) => self.global_args.push(arg),
                        _ => self.recover_to_eol(),
                    }
                }
                // FROM ends the preamble: hand off to the stage loop.
                TokenKind::Keyword(Keyword::From) => return,
                // Anything else before FROM is rejected with a precise message.
                _ => {
                    let what = describe(&tok.kind);
                    self.errors.push(
                        Diagnostic::error(
                            "only ARG may appear before FROM",
                            Annotation::new(tok.span, format!("found {what} before the first FROM")),
                        )
                        .with_hint(
                            "ARG is the only directive allowed before FROM; move this after the FROM line",
                        ),
                    );
                    return;
                }
            }
        }
    }

    fn parse_stage(&mut self) -> Option<Stage> {
        // FROM <ref> [AS <name>]
        let from_kw = self.expect_keyword(
            Keyword::From,
            "expected FROM at start of stage",
            "every UMF stage must begin with a FROM directive",
        )?;
        let stage_start = from_kw.span.start;

        let arg_tok = self.advance_or_err(
            "expected reference after FROM",
            "FROM takes either an OCI image reference (e.g. `debian:bookworm`) or `scratch`",
        )?;
        let arg_ident = match arg_tok.kind {
            TokenKind::Ident(s) => s,
            _ => {
                self.errors.push(
                    Diagnostic::error(
                        "expected reference after FROM",
                        Annotation::new(arg_tok.span, "expected an identifier or `scratch`"),
                    )
                    .with_hint("example: `FROM debian:bookworm`"),
                );
                return None;
            }
        };
        let source = if arg_ident == "scratch" {
            FromSource::Scratch
        } else {
            let oci = self.validated(
                &arg_ident,
                arg_tok.span,
                "FROM",
                "reference",
                // A `${VAR}` / `$VAR` in the ref is accepted now and resolved
                // after build-time substitution.
                OciReference::new_allowing_placeholders,
            )?;
            FromSource::Reference(oci)
        };

        // Optional AS <name>
        let (name, after_name_end) = if self.match_keyword(Keyword::As) {
            let name_tok = self.advance_or_err(
                "expected stage name after AS",
                "use a bare identifier — e.g. `FROM debian:bookworm AS builder`",
            )?;
            let TokenKind::Ident(name_text) = name_tok.kind else {
                self.errors.push(Diagnostic::error(
                    "expected stage name after AS",
                    Annotation::new(name_tok.span, "expected an identifier"),
                ));
                return None;
            };
            let end = name_tok.span.end;
            let name = self.validated(
                &name_text,
                name_tok.span,
                "AS",
                "stage name",
                StageName::new,
            )?;
            (Some(name), end)
        } else {
            (None, arg_tok.span.end)
        };

        let from = FromArg {
            source,
            span: Span::new(from_kw.span.start, after_name_end),
        };

        self.expect_newline_after("FROM");

        // Per-stage directives until next FROM or EOF.
        let mut directives = Vec::new();
        loop {
            self.skip_newlines();
            match self.peek_kind() {
                None | Some(TokenKind::Keyword(Keyword::From)) => break,
                _ => {
                    if let Some(d) = self.parse_directive() {
                        directives.push(d);
                        // A multi-item line (`LABEL a=1 b=2`, `EXPOSE 80 443`)
                        // queued its remaining items; flush them in source order.
                        directives.append(&mut self.pending);
                    } else {
                        self.recover_to_eol();
                    }
                }
            }
            // Stop a single giant stage of invalid directives from collecting
            // an unbounded diagnostic set (memory/time DoS).
            if self.errors.len() > diagnostics::MAX_COLLECTED_DIAGNOSTICS {
                break;
            }
        }

        let stage_end = self.last_consumed_end().max(after_name_end);
        Some(Stage {
            from,
            name,
            directives,
            span: Span::new(stage_start, stage_end),
        })
    }

    // ----- per-directive dispatch -----

    fn parse_directive(&mut self) -> Option<Directive> {
        let tok = self.peek()?.clone();
        let TokenKind::Keyword(kw) = tok.kind else {
            // A non-keyword leading token. If it names a well-known Dockerfile
            // directive umf deliberately doesn't support, warn and skip the line
            // (the caller's `recover_to_eol` consumes it) so a single-source
            // Containerfile still builds; otherwise it's a genuine syntax error.
            if let TokenKind::Ident(name) = &tok.kind
                && let Some(hint) = unsupported_dockerfile_directive(name)
            {
                self.warnings.push(
                    Diagnostic::warning(
                        format!("`{name}` is not supported by umf and is ignored"),
                        Annotation::new(tok.span, "unsupported directive, skipped"),
                    )
                    .with_hint(hint),
                );
                return None;
            }
            self.errors.push(Diagnostic::error(
                "expected a directive keyword",
                Annotation::new(tok.span, format!("found {} here", describe(&tok.kind))),
            ));
            return None;
        };
        self.advance(); // consume the keyword
        let start = tok.span.start;

        match kw {
            Keyword::From => {
                self.errors.push(Diagnostic::error(
                    "unexpected FROM inside a stage",
                    Annotation::new(tok.span, "FROM starts a new stage and cannot be nested"),
                ));
                None
            }
            Keyword::As => {
                self.errors.push(Diagnostic::error(
                    "AS is only valid in a FROM clause",
                    Annotation::new(tok.span, "remove or move into the FROM line"),
                ));
                None
            }
            Keyword::Label => self.parse_label(start),
            Keyword::Env => self.parse_env(start),
            Keyword::Arg => self.parse_arg(start),
            Keyword::Bootloader => self.removed_directive(
                tok.span,
                "BOOTLOADER",
                "set boot packaging with `LABEL org.imagilux.umf.flavor=systemd-boot|uki` instead",
            ),
            Keyword::Rootfs => self.removed_directive(
                tok.span,
                "ROOTFS",
                "add the userland with `ADD --from=<ref> / /` instead",
            ),
            Keyword::Shell => self.parse_shell(start),
            Keyword::User => self.parse_user(start),
            Keyword::Workdir => self.parse_workdir(start),
            Keyword::Run => self.parse_run(start),
            Keyword::Add => self.parse_add(start, false),
            Keyword::Copy => self.parse_add(start, true),
            Keyword::Entrypoint => self.parse_entrypoint(start),
            Keyword::Expose => self.parse_expose(start),
            Keyword::Enable => self.removed_directive(
                tok.span,
                "ENABLE",
                "enable the unit directly instead: `ADD`/`COPY` the `.service` file (or let the \
                 package manager place it), or `RUN systemctl enable <unit>`",
            ),
            Keyword::Disable => self.removed_directive(
                tok.span,
                "DISABLE",
                "act on the unit directly instead: remove/mask it, or `RUN systemctl disable <unit>`",
            ),
            Keyword::Hostname => self.removed_directive(
                tok.span,
                "HOSTNAME",
                "set the hostname at first boot with cloud-init / ignition instead",
            ),
            Keyword::Locale => self.removed_directive(
                tok.span,
                "LOCALE",
                "set the locale at first boot with cloud-init / ignition instead",
            ),
            Keyword::Timezone => self.removed_directive(
                tok.span,
                "TIMEZONE",
                "set the timezone at first boot with cloud-init / ignition instead",
            ),
            Keyword::Cmd => self.parse_cmd(start),
            Keyword::Volume => self.parse_volume(start),
            Keyword::Stopsignal => self.parse_stopsignal(start),
        }
    }

    // ----- metadata directives -----

    /// `LABEL k=v [k2=v2 …]` — one or more key/value pairs on a line (regular
    /// Docker style). The first pair is returned; any extras are queued, each as
    /// its own `Label` directive (semantically identical, since LABEL just
    /// accumulates into the image config).
    fn parse_label(&mut self, start: usize) -> Option<Directive> {
        let pairs = self.parse_key_value_list("LABEL")?;
        self.expect_newline_after("LABEL");
        let mut built = Vec::with_capacity(pairs.len());
        for (i, (raw_key, raw_value)) in pairs.into_iter().enumerate() {
            let key =
                self.validated(&raw_key.value, raw_key.span, "LABEL", "key", LabelKey::new)?;
            let value = self.validated(
                &raw_value.value,
                raw_value.span,
                "LABEL",
                "value",
                LabelValue::new,
            )?;
            let span = pair_span(start, i, key.span, value.span);
            built.push(Directive::Label(Label { key, value, span }));
        }
        self.take_first_queue_rest(built)
    }

    /// `ENV k=v [k2=v2 …]` — one or more key/value pairs on a line (regular
    /// Docker style). The first pair is returned; extras are queued, each as its
    /// own `Env` directive (semantically identical — ENV applies positionally
    /// and is last-wins per key, both preserved by emitting in source order).
    fn parse_env(&mut self, start: usize) -> Option<Directive> {
        let pairs = self.parse_key_value_list("ENV")?;
        self.expect_newline_after("ENV");
        let mut built = Vec::with_capacity(pairs.len());
        for (i, (raw_key, raw_value)) in pairs.into_iter().enumerate() {
            let key =
                self.validated(&raw_key.value, raw_key.span, "ENV", "key", EnvVarName::new)?;
            let value = self.validated(
                &raw_value.value,
                raw_value.span,
                "ENV",
                "value",
                EnvVarValue::new,
            )?;
            let span = pair_span(start, i, key.span, value.span);
            built.push(Directive::Env(Env { key, value, span }));
        }
        self.take_first_queue_rest(built)
    }

    fn parse_arg(&mut self, start: usize) -> Option<Directive> {
        let name_tok = self.advance_or_err(
            "expected argument name after ARG",
            "ARG takes a name with an optional default — e.g. `ARG VERSION=1.0`",
        )?;
        let TokenKind::Ident(name_text) = name_tok.kind else {
            self.errors.push(Diagnostic::error(
                "expected argument name after ARG",
                Annotation::new(name_tok.span, "expected an identifier"),
            ));
            return None;
        };
        let name = self.validated(&name_text, name_tok.span, "ARG", "name", EnvVarName::new)?;

        let (default, end) = if self.peek_punct() == Some(Punct::Equals) {
            self.advance(); // consume =
            let val_tok = self.advance_or_err(
                "expected default value after `=`",
                "ARG default can be an identifier or quoted string",
            )?;
            let first_text = match &val_tok.kind {
                TokenKind::Ident(s) | TokenKind::String(s) => s.clone(),
                TokenKind::Number(n) => n.to_string(),
                _ => {
                    self.errors.push(Diagnostic::error(
                        "expected default value",
                        Annotation::new(val_tok.span, "expected identifier, number, or string"),
                    ));
                    return None;
                }
            };
            // Splice `1.0`-style dotted/versioned values back together.
            let (val_text, end) = self.splice_value(&val_tok, first_text);
            let span = Span::new(val_tok.span.start, end);
            let default = self.validated(&val_text, span, "ARG", "default", EnvVarValue::new)?;
            (Some(default), end)
        } else {
            (None, name.span.end)
        };

        self.expect_newline_after("ARG");
        let span = Span::new(start, end);
        // Secret-shaped names get a nudge toward the sanctioned channel. It is
        // a warning, never a refusal (the user may legitimately know what they
        // are doing) — and the value never enters the image history, config, or
        // cache key regardless, so this is about steering, not enforcement.
        if name_looks_secret(name.value.as_str()) {
            self.warnings.push(
                Diagnostic::warning(
                    format!(
                        "`ARG {}` has a secret-shaped name; a build arg is not a secret channel",
                        name.value,
                    ),
                    Annotation::new(span, "secret-shaped ARG name"),
                )
                .with_hint(
                    "for sensitive material use `RUN --mount=type=secret,id=…`: it never enters a layer, the image history, or the cache key",
                ),
            );
        }
        Some(Directive::Arg(Arg {
            name,
            default,
            span,
        }))
    }

    // ----- removed directives -----

    /// A directive keyword removed in a given spec revision. Emits a migration
    /// diagnostic naming the version and the replacement, then returns `None` so
    /// the caller recovers to end-of-line (the value tokens are skipped, not
    /// reparsed).
    fn removed_directive(
        &mut self,
        span: Span,
        name: &str,
        replacement: &str,
    ) -> Option<Directive> {
        self.errors.push(
            Diagnostic::error(
                format!("`{name}` is not a UMF directive"),
                Annotation::new(span, "not a directive"),
            )
            .with_hint(replacement),
        );
        None
    }

    // ----- build-step directives -----

    /// `SHELL` has two additive forms, both honouring regular Docker style:
    ///
    /// * keyword: `SHELL bash` (umf shorthand), resolved to its conventional
    ///   argv at parse time.
    /// * exec array: `SHELL ["/bin/bash", "-euo", "pipefail", "-c"]` (the Docker
    ///   form), whose argv is preserved verbatim.
    ///
    /// Both end up as an explicit argv on the [`Shell`] node, so downstream
    /// targets consume one representation.
    fn parse_shell(&mut self, start: usize) -> Option<Directive> {
        let (argv, end) = if matches!(self.peek_kind(), Some(TokenKind::Punct(Punct::LBracket))) {
            // Docker exec form: argv verbatim (at least one element).
            self.parse_string_array("SHELL")?
        } else {
            let (text, span) = self.expect_ident_arg("SHELL")?;
            let argv = match text.as_str() {
                "bash" => shell_keyword_argv(&["/bin/bash", "-c"], span),
                "sh" => shell_keyword_argv(&["/bin/sh", "-c"], span),
                "powershell" => shell_keyword_argv(&["powershell", "-command"], span),
                "none" => Vec::new(),
                other => {
                    self.errors.push(Diagnostic::error(
                        format!("unknown SHELL value `{other}`"),
                        Annotation::new(
                            span,
                            "SHELL accepts `bash`, `sh`, `powershell`, `none`, or an exec array \
                             like `[\"/bin/bash\", \"-c\"]`",
                        ),
                    ));
                    return None;
                }
            };
            (argv, span.end)
        };
        self.expect_newline_after("SHELL");
        Some(Directive::Shell(Shell {
            argv,
            span: Span::new(start, end),
        }))
    }

    fn parse_user(&mut self, start: usize) -> Option<Directive> {
        let (text, span) = self.expect_ident_arg("USER")?;
        let name = self.validated(&text, span, "USER", "name", Username::new)?;
        self.expect_newline_after("USER");
        Some(Directive::User(User {
            name,
            span: Span::new(start, span.end),
        }))
    }

    fn parse_workdir(&mut self, start: usize) -> Option<Directive> {
        let (text, span) = self.expect_ident_arg("WORKDIR")?;
        let path = self.validated(&text, span, "WORKDIR", "path", RecipePath::new)?;
        self.expect_newline_after("WORKDIR");
        Some(Directive::Workdir(Workdir {
            path,
            span: Span::new(start, span.end),
        }))
    }

    fn parse_run(&mut self, start: usize) -> Option<Directive> {
        // --mount options are captured into Run::mounts; other long options
        // (forward-compat) flow through into the shell command verbatim.
        let mut mounts: Vec<RunMount> = Vec::new();
        while let Some(TokenKind::LongOption { name, .. }) = self.peek_kind() {
            if name != "mount" {
                break;
            }
            let tok = self.advance()?.clone();
            if let TokenKind::LongOption { value, .. } = tok.kind {
                match value {
                    Some(v) => match parse_run_mount_value(&v, tok.span) {
                        Ok(m) => mounts.push(m),
                        Err(diag) => {
                            self.errors.push(diag);
                            return None;
                        }
                    },
                    None => {
                        self.errors.push(
                            Diagnostic::error(
                                "RUN --mount requires a value",
                                Annotation::new(
                                    tok.span,
                                    "expected --mount=type=<kind>,...",
                                ),
                            )
                            .with_hint(
                                "example: `RUN --mount=type=secret,id=k,target=/run/secrets/k cat /run/secrets/k`",
                            ),
                        );
                        return None;
                    }
                }
            }
        }

        let first = self.peek().cloned();
        let Some(first_tok) = first else {
            let span = self.eof_span();
            self.errors.push(Diagnostic::error(
                "expected command after RUN",
                Annotation::new(span, "RUN must be followed by a command"),
            ));
            return None;
        };
        if matches!(first_tok.kind, TokenKind::Newline) {
            self.errors.push(Diagnostic::error(
                "expected command after RUN",
                Annotation::new(first_tok.span, "RUN must be followed by a command"),
            ));
            return None;
        }
        let cmd_start = first_tok.span.start;
        let mut cmd_end = cmd_start;
        while let Some(tok) = self.peek() {
            if matches!(tok.kind, TokenKind::Newline) {
                break;
            }
            cmd_end = tok.span.end;
            self.advance();
        }
        // Slice from the original source rather than rebuilding from tokens —
        // preserves whitespace, quoting, and the `\\\n` line continuations
        // that shell will interpret on its own.
        let raw = self.source[cmd_start..cmd_end].to_string();
        let cmd_span = Span::new(cmd_start, cmd_end);
        self.expect_newline_after("RUN");
        Some(Directive::Run(Run {
            command: RunCommand::Shell(Spanned::new(raw, cmd_span)),
            mounts,
            span: Span::new(start, cmd_end),
        }))
    }

    /// Parse `ADD` (`plain_copy == false`) or `COPY` (`plain_copy == true`).
    /// Both share this parser and the same AST node; the verb only changes the
    /// diagnostics text and the `plain_copy` flag, which the builder uses to
    /// reject remote (URL / OCI) sources for `COPY`.
    fn parse_add(&mut self, start: usize, plain_copy: bool) -> Option<Directive> {
        let verb = if plain_copy { "COPY" } else { "ADD" };
        // Long options: capture --from=<stage>; drop unrecognised options so
        // forward-compatible parsing doesn't fail on future flags.
        // `--from=<stage>` roots the source in a sibling build stage. An
        // external OCI image is an OCI *source* (`ADD <image-ref> /`), not a
        // `--from`. Unrecognised long options are dropped (forward-compatible).
        let mut from: Option<Spanned<StageName>> = None;
        while let Some(TokenKind::LongOption { .. }) = self.peek_kind() {
            let tok = self.advance()?.clone();
            if let TokenKind::LongOption { name, value } = tok.kind
                && name == "from"
            {
                let Some(v) = value else {
                    self.errors.push(
                        Diagnostic::error(
                            format!("{verb} --from requires a value"),
                            Annotation::new(tok.span, "expected --from=<stage>"),
                        )
                        .with_hint(format!("example: `{verb} --from=builder /src /dst`")),
                    );
                    return None;
                };
                from = Some(self.validated(
                    &v,
                    tok.span,
                    verb,
                    "--from stage name",
                    StageName::new,
                )?);
            }
        }
        let src_tok = self.advance_or_err(
            &format!("expected source after {verb}"),
            &format!("{verb} takes <source> <destination>"),
        )?;
        let src_text = match src_tok.kind {
            TokenKind::Ident(s) | TokenKind::String(s) => s,
            _ => {
                self.errors.push(Diagnostic::error(
                    format!("expected source path after {verb}"),
                    Annotation::new(src_tok.span, "expected identifier or string"),
                ));
                return None;
            }
        };
        // Classify the source by scheme and shape (see `AddSource`):
        //   oci:// | https+oci://        -> explicit OCI image
        //   http:// | https://           -> remote blob (URL)
        //   ./ ../ / ~ prefix            -> local path
        //   bare ref with :tag / @digest -> OCI image (shape heuristic)
        //   otherwise                    -> local path
        // `COPY` accepts the same forms here; the builder rejects the remote
        // ones (URL / OCI) at lowering time so the diagnostic can point at the
        // resolved layer, not just the surface syntax.
        // A `${VAR}` / `$VAR` in an OCI ref or URL is accepted now and resolved
        // after build-time substitution, mirroring `FROM`. Keeping
        // the parser lenient here (rather than strict-now / build-fail-later)
        // means `ADD repo:${TAG} /` and `ADD https://host/${VER}.tar /` parse.
        let source = if let Some(rest) = src_text
            .strip_prefix("oci://")
            .or_else(|| src_text.strip_prefix("https+oci://"))
        {
            AddSource::Oci(self.validated(
                rest,
                src_tok.span,
                verb,
                "OCI image reference",
                OciReference::new_allowing_placeholders,
            )?)
        } else if src_text.starts_with("http://") || src_text.starts_with("https://") {
            AddSource::Url(self.validated(
                &src_text,
                src_tok.span,
                verb,
                "source URL",
                HttpsUrl::new_allowing_placeholders,
            )?)
        } else if is_local_path(&src_text) {
            AddSource::Path(Spanned::new(src_text, src_tok.span))
        } else if looks_like_oci_ref(&src_text) {
            AddSource::Oci(self.validated(
                &src_text,
                src_tok.span,
                verb,
                "OCI image reference",
                OciReference::new_allowing_placeholders,
            )?)
        } else {
            AddSource::Path(Spanned::new(src_text, src_tok.span))
        };

        let dst_tok = self.advance_or_err(
            "expected destination after source",
            &format!("{verb} takes <source> <destination>"),
        )?;
        let dst_text = match dst_tok.kind {
            TokenKind::Ident(s) | TokenKind::String(s) => s,
            _ => {
                self.errors.push(Diagnostic::error(
                    "expected destination path after source",
                    Annotation::new(dst_tok.span, "expected identifier or string"),
                ));
                return None;
            }
        };
        let destination = self.validated(
            &dst_text,
            dst_tok.span,
            verb,
            "destination",
            RecipePath::new,
        )?;

        let end = destination.span.end;
        self.expect_newline_after(verb);
        Some(Directive::Add(Add {
            source,
            destination,
            from,
            plain_copy,
            span: Span::new(start, end),
        }))
    }

    // ----- runtime-config directives -----

    fn parse_entrypoint(&mut self, start: usize) -> Option<Directive> {
        // Polymorphic: keyword (systemd/openrc/none), shell-form
        // path starting with `/`, or JSON-array exec form `["argv0", ...]`.
        let first_kind = match self.peek_kind() {
            None | Some(TokenKind::Newline) => {
                let span = self.current_or_eof_span();
                self.errors.push(
                    Diagnostic::error(
                        "expected argument after ENTRYPOINT",
                        Annotation::new(span, "ENTRYPOINT requires a value"),
                    )
                    .with_hint(
                        "accepted: `systemd` | `openrc` | `none` | `/path/to/bin [args]` | `[\"argv0\", ...]`",
                    ),
                );
                return None;
            }
            Some(k) => k.clone(),
        };

        let (init, end) = match first_kind {
            TokenKind::Punct(Punct::LBracket) => self.parse_entrypoint_exec()?,
            TokenKind::Ident(s) if s.starts_with('/') => self.parse_entrypoint_shell_path(),
            TokenKind::Ident(s) => {
                let tok = self.advance()?;
                let init = match s.as_str() {
                    "systemd" => EntrypointInit::Systemd,
                    "openrc" => EntrypointInit::OpenRc,
                    "none" => EntrypointInit::None,
                    other => {
                        self.errors.push(
                            Diagnostic::error(
                                format!("unknown ENTRYPOINT value `{other}`"),
                                Annotation::new(
                                    tok.span,
                                    "ENTRYPOINT keyword must be `systemd`, `openrc`, or `none`",
                                ),
                            )
                            .with_hint(
                                "to run a binary as PID 1, use a leading `/` (e.g. `ENTRYPOINT /myapp`) or exec form `ENTRYPOINT [\"/myapp\", ...]`",
                            ),
                        );
                        return None;
                    }
                };
                (init, tok.span.end)
            }
            _ => {
                let tok = self.advance()?;
                self.errors.push(Diagnostic::error(
                    "expected ENTRYPOINT argument",
                    Annotation::new(
                        tok.span,
                        "expected `systemd`/`openrc`/`none`, a `/path`, or exec form",
                    ),
                ));
                return None;
            }
        };

        self.expect_newline_after("ENTRYPOINT");
        Some(Directive::Entrypoint(Entrypoint {
            init,
            span: Span::new(start, end),
        }))
    }

    /// Shell-form ENTRYPOINT: a leading `/path` plus optional args. Slurps the
    /// rest of the logical line as a single command string, like `RUN`.
    fn parse_entrypoint_shell_path(&mut self) -> (EntrypointInit, usize) {
        // Caller has already established that peek is an Ident starting with `/`.
        // We slurp from the current token's start to the line terminator.
        let cmd_start = self
            .peek()
            .map(|t| t.span.start)
            .unwrap_or_else(|| self.eof_span().start);
        let mut cmd_end = cmd_start;
        while let Some(tok) = self.peek() {
            if matches!(tok.kind, TokenKind::Newline) {
                break;
            }
            cmd_end = tok.span.end;
            self.advance();
        }
        let raw = self.source[cmd_start..cmd_end].to_string();
        (
            EntrypointInit::Path(Spanned::new(raw, Span::new(cmd_start, cmd_end))),
            cmd_end,
        )
    }

    /// Exec-form ENTRYPOINT: `["argv0", "argv1", ...]`. Caller has already
    /// confirmed the next token is `[`. Returns the argv vector and the span
    /// end after the closing `]`.
    fn parse_entrypoint_exec(&mut self) -> Option<(EntrypointInit, usize)> {
        let lbracket = self.advance()?; // consume `[`
        let mut argv: Vec<Spanned<String>> = Vec::new();

        // An immediate `]` is an error — empty argv is meaningless.
        if matches!(self.peek_kind(), Some(TokenKind::Punct(Punct::RBracket))) {
            let tok = self.advance()?;
            self.errors.push(Diagnostic::error(
                "ENTRYPOINT exec form requires at least one argument",
                Annotation::new(
                    Span::new(lbracket.span.start, tok.span.end),
                    "expected a quoted string before `]`",
                ),
            ));
            return None;
        }

        let end = loop {
            let tok = self.advance_or_err(
                "expected string in ENTRYPOINT exec form",
                "ENTRYPOINT exec form: [\"argv0\", \"argv1\", ...]",
            )?;
            match tok.kind {
                TokenKind::String(s) => argv.push(Spanned::new(s, tok.span)),
                _ => {
                    self.errors.push(Diagnostic::error(
                        "expected quoted string in ENTRYPOINT exec form",
                        Annotation::new(tok.span, "each argv entry must be a quoted string"),
                    ));
                    return None;
                }
            }

            let sep = self.advance_or_err(
                "expected `,` or `]` in ENTRYPOINT exec form",
                "ENTRYPOINT exec form: [\"argv0\", \"argv1\", ...]",
            )?;
            match sep.kind {
                TokenKind::Punct(Punct::Comma) => continue,
                TokenKind::Punct(Punct::RBracket) => break sep.span.end,
                _ => {
                    self.errors.push(Diagnostic::error(
                        "expected `,` or `]` in ENTRYPOINT exec form",
                        Annotation::new(sep.span, "unexpected token here"),
                    ));
                    return None;
                }
            }
        };

        Some((EntrypointInit::Exec(argv), end))
    }

    /// Parse a JSON-style array of quoted strings `["a", "b", ...]`. The caller
    /// has confirmed the next token is `[`. Returns the strings and the span end
    /// past the closing `]`. Shared by `CMD` exec form and `VOLUME` array form;
    /// `directive` only flavours the diagnostics.
    fn parse_string_array(&mut self, directive: &str) -> Option<(Vec<Spanned<String>>, usize)> {
        let lbracket = self.advance()?; // consume `[`
        let mut items: Vec<Spanned<String>> = Vec::new();

        if matches!(self.peek_kind(), Some(TokenKind::Punct(Punct::RBracket))) {
            let tok = self.advance()?;
            self.errors.push(Diagnostic::error(
                format!("{directive} array form requires at least one element"),
                Annotation::new(
                    Span::new(lbracket.span.start, tok.span.end),
                    "expected a quoted string before `]`",
                ),
            ));
            return None;
        }

        let end = loop {
            let tok = self.advance_or_err(
                &format!("expected string in {directive} array form"),
                "array form: [\"a\", \"b\", ...]",
            )?;
            match tok.kind {
                TokenKind::String(s) => items.push(Spanned::new(s, tok.span)),
                _ => {
                    self.errors.push(Diagnostic::error(
                        format!("expected quoted string in {directive} array form"),
                        Annotation::new(tok.span, "each element must be a quoted string"),
                    ));
                    return None;
                }
            }
            let sep = self.advance_or_err(
                &format!("expected `,` or `]` in {directive} array form"),
                "array form: [\"a\", \"b\", ...]",
            )?;
            match sep.kind {
                TokenKind::Punct(Punct::Comma) => continue,
                TokenKind::Punct(Punct::RBracket) => break sep.span.end,
                _ => {
                    self.errors.push(Diagnostic::error(
                        format!("expected `,` or `]` in {directive} array form"),
                        Annotation::new(sep.span, "unexpected token here"),
                    ));
                    return None;
                }
            }
        };
        Some((items, end))
    }

    /// `CMD <command>` (shell form, slurped to end-of-line) or `CMD ["a", ...]`
    /// (exec form). Container-only; the OCI `Cmd` config field.
    fn parse_cmd(&mut self, start: usize) -> Option<Directive> {
        let (command, end) = match self.peek_kind() {
            None | Some(TokenKind::Newline) => {
                let span = self.current_or_eof_span();
                self.errors.push(
                    Diagnostic::error(
                        "expected a command after CMD",
                        Annotation::new(span, "CMD requires a value"),
                    )
                    .with_hint(
                        "`CMD <command>` (shell form) or `CMD [\"argv0\", ...]` (exec form)",
                    ),
                );
                return None;
            }
            Some(TokenKind::Punct(Punct::LBracket)) => {
                let (argv, end) = self.parse_string_array("CMD")?;
                (CmdForm::Exec(argv), end)
            }
            Some(_) => {
                let cmd_start = self
                    .peek()
                    .map(|t| t.span.start)
                    .unwrap_or_else(|| self.eof_span().start);
                let mut cmd_end = cmd_start;
                while let Some(tok) = self.peek() {
                    if matches!(tok.kind, TokenKind::Newline) {
                        break;
                    }
                    cmd_end = tok.span.end;
                    self.advance();
                }
                let raw = self.source[cmd_start..cmd_end].to_string();
                (
                    CmdForm::Shell(Spanned::new(raw, Span::new(cmd_start, cmd_end))),
                    cmd_end,
                )
            }
        };
        self.expect_newline_after("CMD");
        Some(Directive::Cmd(Cmd {
            command,
            span: Span::new(start, end),
        }))
    }

    /// `VOLUME /a /b` (space-separated) or `VOLUME ["/a", "/b"]` (array form).
    /// Container-only; the OCI `Volumes` config field.
    fn parse_volume(&mut self, start: usize) -> Option<Directive> {
        let (paths, end) = match self.peek_kind() {
            None | Some(TokenKind::Newline) => {
                let span = self.current_or_eof_span();
                self.errors.push(Diagnostic::error(
                    "expected a path after VOLUME",
                    Annotation::new(span, "VOLUME requires at least one mount point"),
                ));
                return None;
            }
            Some(TokenKind::Punct(Punct::LBracket)) => self.parse_string_array("VOLUME")?,
            Some(_) => {
                let mut paths: Vec<Spanned<String>> = Vec::new();
                let mut end = start;
                while let Some(tok) = self.peek() {
                    match &tok.kind {
                        TokenKind::Newline => break,
                        TokenKind::Ident(s) | TokenKind::String(s) => {
                            let value = s.clone();
                            let span = tok.span;
                            paths.push(Spanned::new(value, span));
                            end = span.end;
                            self.advance();
                        }
                        _ => {
                            self.errors.push(Diagnostic::error(
                                "expected a path after VOLUME",
                                Annotation::new(tok.span, "VOLUME takes mount-point paths"),
                            ));
                            return None;
                        }
                    }
                }
                (paths, end)
            }
        };
        self.expect_newline_after("VOLUME");
        Some(Directive::Volume(Volume {
            paths,
            span: Span::new(start, end),
        }))
    }

    /// `STOPSIGNAL SIGTERM` or `STOPSIGNAL 15`. Container-only; the OCI
    /// `StopSignal` config field.
    fn parse_stopsignal(&mut self, start: usize) -> Option<Directive> {
        let tok = self.advance_or_err(
            "expected a signal after STOPSIGNAL",
            "e.g. `STOPSIGNAL SIGTERM` or `STOPSIGNAL 15`",
        )?;
        let (signal, span) = match tok.kind {
            TokenKind::Ident(s) => (s, tok.span),
            TokenKind::Number(n) => (n.to_string(), tok.span),
            _ => {
                self.errors.push(Diagnostic::error(
                    "expected a signal after STOPSIGNAL",
                    Annotation::new(tok.span, "expected a signal name or number"),
                ));
                return None;
            }
        };
        self.expect_newline_after("STOPSIGNAL");
        Some(Directive::Stopsignal(Stopsignal {
            signal: Spanned::new(signal, span),
            span: Span::new(start, span.end),
        }))
    }

    /// `EXPOSE <port>[/proto] [<port2>[/proto] …]` — one or more ports on a line
    /// (regular Docker style: `EXPOSE 80 443`). The protocol is optional and
    /// defaults to `tcp` (`EXPOSE 8080` == `EXPOSE 8080/tcp`). The first port is
    /// returned; extra ports are queued as their own `Expose` directives.
    fn parse_expose(&mut self, start: usize) -> Option<Directive> {
        let mut built = Vec::new();
        loop {
            let (port, protocol, espan) = self.parse_expose_entry()?;
            let span = if built.is_empty() {
                Span::new(start, espan.end)
            } else {
                espan
            };
            built.push(Directive::Expose(Expose {
                port,
                protocol,
                span,
            }));
            // Another port follows only if the next token is a number.
            if !matches!(self.peek_kind(), Some(TokenKind::Number(_))) {
                break;
            }
        }
        self.expect_newline_after("EXPOSE");
        self.take_first_queue_rest(built)
    }

    /// Parse one `EXPOSE` entry: a port with an optional `/protocol` (default
    /// tcp). Returns the port, protocol, and the entry's source span.
    fn parse_expose_entry(&mut self) -> Option<(u16, ExposeProtocol, Span)> {
        let port_tok = self.advance_or_err(
            "expected port number after EXPOSE",
            "EXPOSE takes `<port>[/<protocol>]` — e.g. `EXPOSE 80` or `EXPOSE 80/tcp`",
        )?;
        let (port, port_span) = match port_tok.kind {
            TokenKind::Number(n) if n <= u64::from(u16::MAX) => (n as u16, port_tok.span),
            TokenKind::Number(n) => {
                self.errors.push(Diagnostic::error(
                    format!("port {n} out of range"),
                    Annotation::new(port_tok.span, "ports must fit in u16 (0..=65535)"),
                ));
                return None;
            }
            _ => {
                self.errors.push(Diagnostic::error(
                    "expected port number after EXPOSE",
                    Annotation::new(port_tok.span, "expected a number"),
                ));
                return None;
            }
        };

        // Protocol is optional (regular Docker `EXPOSE 80` defaults to tcp). The
        // lexer glues `/tcp` to the port as a separate `/tcp` ident, so a
        // protocol is present only when the next token is an ident starting `/`.
        let has_proto = matches!(self.peek_kind(), Some(TokenKind::Ident(s)) if s.starts_with('/'));
        if !has_proto {
            return Some((port, ExposeProtocol::Tcp, port_span));
        }
        let proto_tok = self.advance_or_err(
            "expected `/<protocol>` after port",
            "EXPOSE syntax: `EXPOSE <port>/<protocol>`",
        )?;
        let TokenKind::Ident(proto_text) = proto_tok.kind else {
            self.errors.push(Diagnostic::error(
                "expected `/<protocol>` after port",
                Annotation::new(proto_tok.span, "expected `/tcp` or `/udp`"),
            ));
            return None;
        };
        let stripped = proto_text.strip_prefix('/').unwrap_or(&proto_text);
        let protocol = match stripped {
            "tcp" => ExposeProtocol::Tcp,
            "udp" => ExposeProtocol::Udp,
            other => {
                self.errors.push(Diagnostic::error(
                    format!("unknown protocol `{other}`"),
                    Annotation::new(proto_tok.span, "EXPOSE accepts `/tcp` or `/udp`"),
                ));
                return None;
            }
        };
        Some((
            port,
            protocol,
            Span::new(port_span.start, proto_tok.span.end),
        ))
    }

    // ----- shared helpers -----

    /// Run a newtype constructor over `raw` (the original lexed text whose
    /// source span is `token_span`) and wrap the result in [`Spanned`]. On
    /// failure pushes a [`Diagnostic`] whose primary annotation is a sub-span
    /// of `token_span` derived from the validation error's byte offset.
    /// Reconstruct a directive value the lexer split across adjacent tokens.
    ///
    /// `ARG VERSION=1.0` lexes as `Number(1)` + `Ident(".0")` (a digit run is a
    /// number; `.` then starts a word), and `1.2.3` similarly. For a *value*
    /// (the right-hand side of an `ARG` / `ENV` / `LABEL` `=`), splice every
    /// **immediately adjacent** Ident / Number token (no intervening
    /// whitespace) onto the first and return the original source text of the
    /// whole run. A quoted string is the author's explicit value and is taken
    /// verbatim, never spliced.
    ///
    /// This is scoped to value reads, so the `EXPOSE 80/tcp` tokenization
    /// (which relies on `80` and `/tcp` staying separate) is unaffected.
    fn splice_value(&mut self, first: &Token, first_text: String) -> (String, usize) {
        if matches!(first.kind, TokenKind::String(_)) {
            return (first_text, first.span.end);
        }
        let start = first.span.start;
        let mut end = first.span.end;
        while let Some(next) = self.peek() {
            if next.span.start == end
                && matches!(next.kind, TokenKind::Ident(_) | TokenKind::Number(_))
            {
                end = next.span.end;
                self.advance();
            } else {
                break;
            }
        }
        (self.source[start..end].to_string(), end)
    }

    fn validated<T, E: ValidationError>(
        &mut self,
        raw: &str,
        token_span: Span,
        directive: &str,
        arg_label: &str,
        constructor: impl FnOnce(String) -> Result<T, E>,
    ) -> Option<Spanned<T>> {
        match constructor(raw.to_string()) {
            Ok(v) => Some(Spanned::new(v, token_span)),
            Err(err) => {
                self.errors.push(diagnostics::from_validation_error(
                    &err, raw, token_span, directive, arg_label,
                ));
                None
            }
        }
    }

    /// Lex one `KEY=VALUE` pair and return the raw text + span for each side.
    /// The caller is responsible for running the appropriate newtype
    /// constructor on the raw strings.
    fn parse_key_value(
        &mut self,
        directive: &str,
    ) -> Option<(Spanned<String>, Spanned<String>, usize)> {
        let key_tok = self.advance_or_err(
            &format!("expected key after {directive}"),
            "syntax: `KEY=VALUE`",
        )?;
        let key_text = match key_tok.kind {
            TokenKind::Ident(s) => s,
            _ => {
                self.errors.push(Diagnostic::error(
                    format!("expected key after {directive}"),
                    Annotation::new(key_tok.span, "expected an identifier"),
                ));
                return None;
            }
        };
        let key = Spanned::new(key_text, key_tok.span);

        let eq_tok =
            self.advance_or_err("expected `=` between key and value", "syntax: `KEY=VALUE`")?;
        if !matches!(eq_tok.kind, TokenKind::Punct(Punct::Equals)) {
            self.errors.push(Diagnostic::error(
                "expected `=` between key and value",
                Annotation::new(eq_tok.span, "missing `=`"),
            ));
            return None;
        }

        let val_tok = self.advance_or_err(
            "expected value after `=`",
            "value may be an identifier, number, or quoted string",
        )?;
        let first_text = match &val_tok.kind {
            TokenKind::Ident(s) | TokenKind::String(s) => s.clone(),
            TokenKind::Number(n) => n.to_string(),
            _ => {
                self.errors.push(Diagnostic::error(
                    "expected value after `=`",
                    Annotation::new(val_tok.span, "expected identifier, number, or string"),
                ));
                return None;
            }
        };
        // Splice `1.0`-style dotted/versioned values back together (`ENV V=1.0`).
        let (val_text, end) = self.splice_value(&val_tok, first_text);
        let value = Spanned::new(val_text, Span::new(val_tok.span.start, end));
        Some((key, value, end))
    }

    /// Parse one or more `key=value` pairs on a single directive line (regular
    /// Docker `LABEL a=1 b=2` / `ENV a=1 b=2`). Pairs are whitespace-separated;
    /// the list ends at the newline. Returns the raw (unvalidated) pairs, always
    /// at least one.
    fn parse_key_value_list(
        &mut self,
        directive: &str,
    ) -> Option<Vec<(Spanned<String>, Spanned<String>)>> {
        let mut pairs = Vec::new();
        loop {
            let (key, value, _end) = self.parse_key_value(directive)?;
            pairs.push((key, value));
            // Another pair follows only if the next token starts a new key
            // (an identifier). A newline / EOF / anything else ends the list.
            if !matches!(self.peek_kind(), Some(TokenKind::Ident(_))) {
                break;
            }
        }
        Some(pairs)
    }

    /// Return the first directive a multi-item line produced and queue the rest
    /// in `self.pending` for the stage loop to flush in source order. `built` is
    /// always non-empty (the list parsers yield at least one item).
    fn take_first_queue_rest(&mut self, built: Vec<Directive>) -> Option<Directive> {
        let mut iter = built.into_iter();
        let first = iter.next()?;
        self.pending.extend(iter);
        Some(first)
    }

    fn expect_ident_arg(&mut self, directive: &str) -> Option<(String, Span)> {
        let tok = self.advance_or_err(
            &format!("expected argument after {directive}"),
            "see the spec for accepted values",
        )?;
        match tok.kind {
            TokenKind::Ident(s) => Some((s, tok.span)),
            _ => {
                self.errors.push(Diagnostic::error(
                    format!("expected argument after {directive}"),
                    Annotation::new(tok.span, "expected an identifier"),
                ));
                None
            }
        }
    }

    // ----- low-level token helpers -----

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn peek_kind(&self) -> Option<&TokenKind> {
        self.peek().map(|t| &t.kind)
    }

    fn peek_punct(&self) -> Option<Punct> {
        match self.peek_kind() {
            Some(TokenKind::Punct(p)) => Some(*p),
            _ => None,
        }
    }

    fn advance(&mut self) -> Option<Token> {
        let tok = self.tokens.get(self.pos).cloned()?;
        self.pos += 1;
        Some(tok)
    }

    fn advance_or_err(&mut self, message: &str, hint: &str) -> Option<Token> {
        match self.advance() {
            Some(tok) if !matches!(tok.kind, TokenKind::Newline) => Some(tok),
            Some(_eol) => {
                let span = self.eof_span();
                self.errors.push(
                    Diagnostic::error(message, Annotation::new(span, "unexpected end of line"))
                        .with_hint(hint),
                );
                None
            }
            None => {
                let span = self.eof_span();
                self.errors.push(
                    Diagnostic::error(message, Annotation::new(span, "unexpected end of file"))
                        .with_hint(hint),
                );
                None
            }
        }
    }

    fn match_keyword(&mut self, kw: Keyword) -> bool {
        if matches!(self.peek_kind(), Some(TokenKind::Keyword(k)) if *k == kw) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_keyword(&mut self, kw: Keyword, message: &str, hint: &str) -> Option<Token> {
        match self.peek() {
            Some(tok) if matches!(tok.kind, TokenKind::Keyword(k) if k == kw) => self.advance(),
            _ => {
                let span = self.current_or_eof_span();
                self.errors
                    .push(Diagnostic::error(message, Annotation::new(span, hint)));
                None
            }
        }
    }

    fn expect_newline_after(&mut self, directive: &str) {
        if let Some(tok) = self.peek() {
            if !matches!(tok.kind, TokenKind::Newline) {
                let span = tok.span;
                self.errors.push(Diagnostic::error(
                    format!("unexpected extra arguments for {directive}"),
                    Annotation::new(span, "expected end of line"),
                ));
                self.recover_to_eol();
            }
        }
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek_kind(), Some(TokenKind::Newline)) {
            self.advance();
        }
    }

    fn recover_to_eol(&mut self) {
        while let Some(tok) = self.peek() {
            if matches!(tok.kind, TokenKind::Newline) {
                self.advance();
                return;
            }
            self.advance();
        }
    }

    fn recover_to_next_stage(&mut self) {
        while let Some(tok) = self.peek() {
            if matches!(tok.kind, TokenKind::Keyword(Keyword::From)) {
                return;
            }
            self.advance();
        }
    }

    fn current_or_eof_span(&self) -> Span {
        self.peek().map_or_else(|| self.eof_span(), |t| t.span)
    }

    fn eof_span(&self) -> Span {
        self.tokens
            .last()
            .map_or(Span::new(0, 0), |t| Span::new(t.span.end, t.span.end))
    }

    fn last_consumed_end(&self) -> usize {
        if self.pos == 0 {
            0
        } else {
            self.tokens[self.pos - 1].span.end
        }
    }
}

/// Well-known Dockerfile directives umf deliberately does not support.
///
/// Returns a short migration hint when `name` is one of them, so
/// [`Parser::parse_directive`] can warn and skip the line instead of
/// hard-erroring (keeping single-source Containerfiles buildable). Any other
/// unknown leading token returns `None` and stays a genuine syntax error.
fn unsupported_dockerfile_directive(name: &str) -> Option<&'static str> {
    // CMD / VOLUME / STOPSIGNAL / COPY are real directives
    // (they lex as keywords now and never reach here).
    match name {
        "MAINTAINER" => Some("record authorship with `LABEL org.opencontainers.image.authors=...`"),
        "HEALTHCHECK" => Some("health checks are configured by the runtime, not the recipe"),
        "ONBUILD" => Some("umf has no build triggers; inline the steps in the consuming recipe"),
        _ => None,
    }
}

/// Whether an `ARG` name looks like it carries a secret, so `parse_arg` can
/// nudge the author toward `RUN --mount=type=secret`. Matches Docker's own
/// build-secret guidance: a name containing `PASSWORD`, `TOKEN`, `SECRET`, or
/// `KEY` (case-insensitive). A nudge only — never a refusal.
fn name_looks_secret(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["PASSWORD", "TOKEN", "SECRET", "KEY"]
        .iter()
        .any(|needle| upper.contains(needle))
}

fn describe(kind: &TokenKind) -> &'static str {
    match kind {
        TokenKind::Keyword(_) => "a keyword",
        TokenKind::Ident(_) => "an identifier",
        TokenKind::String(_) => "a string",
        TokenKind::Number(_) => "a number",
        TokenKind::Punct(_) => "punctuation",
        TokenKind::LongOption { .. } => "a long option",
        TokenKind::Newline => "end of line",
    }
}

/// Source span for one item of a multi-item directive line (`LABEL a=1 b=2`).
/// The first item spans from the directive keyword (`start`) through its value;
/// later items span just their own `key`..`value` tokens.
fn pair_span(start: usize, index: usize, key: Span, value: Span) -> Span {
    if index == 0 {
        Span::new(start, value.end)
    } else {
        Span::new(key.start, value.end)
    }
}

/// Build the argv for a keyword `SHELL` form (`SHELL bash` → `["/bin/bash",
/// "-c"]`). The synthesized tokens carry the keyword's source span, since they
/// have no per-token source of their own.
fn shell_keyword_argv(parts: &[&str], span: Span) -> Vec<Spanned<String>> {
    parts
        .iter()
        .map(|p| Spanned::new((*p).to_string(), span))
        .collect()
}

/// An `ADD` source is unambiguously a local path when it is `.`/`..` or starts
/// with `./`, `../`, `/`, or `~/`. Checked before the OCI-ref shape heuristic so
/// a local path containing a `:` (`./weird:name`) isn't mistaken for an image.
fn is_local_path(s: &str) -> bool {
    s == "."
        || s == ".."
        || s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with('/')
        || s.starts_with("~/")
}

/// A bare `ADD` source (no scheme, not path-prefixed) is treated as an OCI image
/// reference when it carries a `:tag` or `@digest` marker. `/` alone is *not* a
/// signal: local paths contain it too. Use `oci://<ref>` for a bare-name image.
fn looks_like_oci_ref(s: &str) -> bool {
    s.contains(':') || s.contains('@')
}

/// Parse the value of a `RUN --mount=...` long option.
///
/// Accepts `type=secret,id=<id>[,target=<path>]`. Other mount kinds are not
/// yet supported and produce a diagnostic. Unknown keys inside the value are
/// silently ignored for forward compatibility.
fn parse_run_mount_value(value: &str, span: Span) -> Result<RunMount, Diagnostic> {
    let mut kind_str: Option<&str> = None;
    let mut id: Option<String> = None;
    let mut target: Option<String> = None;
    for pair in value.split(',') {
        let (key, val) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "type" => kind_str = Some(val),
            "id" => id = Some(val.to_string()),
            "target" => target = Some(val.to_string()),
            _ => {} // forward-compat: unknown key ignored
        }
    }
    match kind_str {
        Some("secret") => {
            let id = id.ok_or_else(|| {
                Diagnostic::error(
                    "RUN --mount=type=secret requires an id",
                    Annotation::new(span, "expected id=<id>"),
                )
                .with_hint("example: `--mount=type=secret,id=signing-key`")
            })?;
            let id = SecretId::new(id.clone()).map_err(|err| {
                diagnostics::from_validation_error(&err, &id, span, "RUN --mount=type=secret", "id")
            })?;
            let target = target
                .map(|t| {
                    RecipePath::new(t.clone())
                        .map(|p| Spanned::new(p, span))
                        .map_err(|err| {
                            diagnostics::from_validation_error(
                                &err,
                                &t,
                                span,
                                "RUN --mount=type=secret",
                                "target",
                            )
                        })
                })
                .transpose()?;
            Ok(RunMount {
                kind: RunMountKind::Secret {
                    id: Spanned::new(id, span),
                    target,
                },
                span,
            })
        }
        Some(other) => Err(Diagnostic::error(
            "unsupported --mount type",
            Annotation::new(span, format!("type={other} is not yet supported")),
        )
        .with_hint("only `type=secret` is implemented in this release")),
        None => Err(Diagnostic::error(
            "RUN --mount requires type=...",
            Annotation::new(span, "expected type=<kind>"),
        )
        .with_hint("example: `--mount=type=secret,id=k`")),
    }
}
