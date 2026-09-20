//! Daemon-only provider discovery layered over shared protocol models.

use std::path::Path;

pub use waku_protocol::model::*;

pub fn provider_probe(provider: ProviderKind, binary_override: Option<&str>) -> ProviderProbe {
    let path = match binary_override {
        Some(binary) => crate::command_env::resolve_binary_override(binary),
        None => provider_binary(provider),
    };
    ProviderProbe {
        provider,
        installed: path.is_some(),
        path,
        models: crate::model_catalog::fallback_models(provider),
        agent_presets: crate::model_catalog::fallback_agent_presets(provider),
    }
}

/// Where a provider's CLI lives when the user has not named one.
///
/// OpenCode 2 is the only provider whose installed name moved inside the
/// window Waku supports. The beta channel shipped `opencode2`, while every
/// later channel — the 2.0.x stable line and its Homebrew formula — installs
/// `opencode`, which is ALSO OpenCode 1's name. `opencode2` is tried first
/// because it can only ever be OpenCode 2; the shared name is accepted only
/// after the binary reports a 2.x version, so a machine with OpenCode 1 alone
/// never advertises its CLI as OpenCode 2 (v1's `serve` takes no `--service`,
/// so accepting it would surface as a start failure instead).
pub(crate) fn provider_binary(provider: ProviderKind) -> Option<std::path::PathBuf> {
    if provider != ProviderKind::OpenCode2 {
        return crate::command_env::find_executable(provider.command());
    }
    crate::command_env::find_executable("opencode2").or_else(|| {
        crate::command_env::find_executable("opencode").filter(|path| is_opencode_v2(path))
    })
}

fn is_opencode_v2(binary: &Path) -> bool {
    probe_provider_version(binary).is_some_and(|version| {
        version
            .split('.')
            .next()
            .and_then(|major| major.parse::<u32>().ok())
            .is_some_and(|major| major >= 2)
    })
}

/// Detect a provider and hydrate its catalog from the daemon-owned cache.
///
/// This is the fast half of stale-while-revalidate: clients can render the
/// last successful catalog immediately, then request live discovery to replace
/// it. Cache I/O stays in the daemon instead of leaking host filesystem access
/// into desktop or Web clients.
pub fn cached_provider_probe(
    provider: ProviderKind,
    binary_override: Option<&str>,
) -> ProviderProbe {
    let cached = crate::model_catalog::cached_models(provider);
    apply_cached_models(provider_probe(provider, binary_override), cached)
}

fn apply_cached_models(
    mut probe: ProviderProbe,
    cached_models: Option<Vec<ProviderModel>>,
) -> ProviderProbe {
    if probe.provider.supports_model_discovery()
        && let Some(models) = cached_models
    {
        probe.models = models;
    }
    probe
}

pub fn discover_provider_models(mut probe: ProviderProbe) -> ProviderProbe {
    if probe.provider.supports_model_discovery()
        && let Some(path) = probe.path.as_deref()
    {
        let (models, agent_presets) = crate::model_catalog::discover_catalog(probe.provider, path);
        probe.models = models;
        probe.agent_presets = agent_presets;
    }
    probe
}

/// Run `<cli> --version` on the daemon host and extract its first version-like
/// token. Provider CLIs decorate this output differently, so clients receive a
/// normalized value rather than subprocess output.
pub fn probe_provider_version(binary: &Path) -> Option<String> {
    let mut command = crate::command_env::command(binary);
    let command = command.arg("--version").stdin(std::process::Stdio::null());
    let output = crate::command_env::output(command).ok()?;
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    parse_cli_version(&combined)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_catalog_replaces_fallback_before_live_discovery() {
        let probe = ProviderProbe {
            provider: ProviderKind::Codex,
            installed: true,
            path: Some("/usr/bin/codex".into()),
            models: crate::model_catalog::fallback_models(ProviderKind::Codex),
            agent_presets: Vec::new(),
        };
        let cached = vec![ProviderModel::new("cached-model", "Cached model").default()];

        let probe = apply_cached_models(probe, Some(cached));

        assert_eq!(probe.models.len(), 1);
        assert_eq!(probe.models[0].id, "cached-model");
    }
}
