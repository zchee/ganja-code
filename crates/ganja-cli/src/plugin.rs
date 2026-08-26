//! `ganja plugin` — the command-line mirror of the plugin store.
//!
//! Spec: the *surface* of Claude Code's `claude plugin` CLI (marketplace
//! add, install, enable/disable, remove, list), over `ganja_core::plugin`'s
//! store. Upstream opencode has no counterpart — the whole plugin system is
//! **D472** — and everything here is a thin door onto the store on purpose:
//! what a subcommand does is exactly what the `/plugin` dialog's action of
//! the same name will do, because both call the same store.
//!
//! Install is explicit and human-typed, and stays that way: no subcommand
//! here installs anything as a side effect of another. A plugin's hooks and
//! MCP servers run with the user's own authority once installed, so the
//! typed `install` is the consent — the same principle that keeps a hook
//! behind the authorship of the config file that declares it.

use anyhow::{Result, bail};
use clap::Subcommand;
use ganja_core::plugin::Store;

/// What `ganja plugin` can be asked to do.
#[derive(Debug, Subcommand)]
pub(crate) enum PluginAction {
    /// List installed plugins: state, origin marketplace, and the components
    /// each contributes.
    ///
    /// The component lines are computed by the same collector the config
    /// loader reads at startup, so what this prints and what a session
    /// serves cannot disagree.
    List,
    /// Manage the marketplaces plugins are installed from.
    Marketplace {
        #[command(subcommand)]
        action: MarketplaceAction,
    },
    /// Install one plugin from an added marketplace, enabled.
    ///
    /// Its skills, agents, hooks, MCP servers and LSP entries join the
    /// config of every project from the next session on. Hooks and servers
    /// run with your own authority — installing is the moment that consent
    /// is given, which is why nothing installs implicitly.
    Install {
        /// Which plugin, spelled `<plugin>@<marketplace>` — the same spelling
        /// Claude Code uses.
        spec: String,
    },
    /// Mark an installed plugin enabled again.
    Enable {
        /// The plugin's name, as `ganja plugin list` shows it.
        plugin: String,
    },
    /// Keep a plugin installed but contributing nothing.
    Disable {
        /// The plugin's name, as `ganja plugin list` shows it.
        plugin: String,
    },
    /// Delete an installed plugin. The marketplace it came from stays added.
    Remove {
        /// The plugin's name, as `ganja plugin list` shows it.
        plugin: String,
    },
    /// Show one installed plugin in full: identity, component inventory, and
    /// the projected token cost of what it puts in front of the model —
    /// Claude Code's own `details` surface.
    Details {
        /// The plugin's name, as `ganja plugin list` shows it.
        plugin: String,
    },
}

/// What `ganja plugin marketplace` can be asked to do.
#[derive(Debug, Subcommand)]
pub(crate) enum MarketplaceAction {
    /// Add a marketplace from a git URL or a local directory.
    ///
    /// The copy is validated — its `.claude-plugin/marketplace.json` must
    /// parse, and every name in it must be one that cannot escape the store
    /// — before anything is kept; a failed add leaves nothing behind.
    Add {
        /// A git URL (anything git clones, a local bare repository
        /// included), or a path to a directory holding the marketplace.
        source: String,
    },
    /// List the added marketplaces: origin, what each offers, and the
    /// installed plugins that came from it.
    List,
    /// Delete an added marketplace's copy and its record.
    ///
    /// Refused while plugins installed from it remain — an installed plugin
    /// would keep working but could never update again, so `ganja plugin
    /// remove` them first is the honest order.
    Remove {
        /// The marketplace's name, as the listing shows it.
        name: String,
    },
    /// Re-fetch added marketplaces from their recorded origins — one by
    /// name, or every one when none is named. Validated exactly as an add
    /// is, and a fetch that renamed itself is refused rather than forked.
    Update {
        /// The marketplace's name; absent, every added marketplace updates.
        name: Option<String>,
    },
}

/// Runs one `ganja plugin` action against the store under the config home.
pub(crate) fn plugin_command(action: PluginAction) -> Result<()> {
    let Some(store) = Store::discover() else {
        bail!("no config home could be resolved, so there is nowhere to keep plugins");
    };

    match action {
        PluginAction::List => list(&store),
        PluginAction::Marketplace {
            action: MarketplaceAction::Add { source },
        } => {
            let name = store.add_marketplace(&source)?;
            println!("added marketplace {name} from {source}");
            Ok(())
        }
        PluginAction::Marketplace {
            action: MarketplaceAction::List,
        } => marketplaces(&store),
        PluginAction::Marketplace {
            action: MarketplaceAction::Remove { name },
        } => {
            store.remove_marketplace(&name)?;
            println!("removed marketplace {name}");
            Ok(())
        }
        PluginAction::Marketplace {
            action: MarketplaceAction::Update { name },
        } => {
            let names = match name {
                Some(name) => vec![name],
                None => store
                    .marketplaces()?
                    .into_iter()
                    .map(|listing| listing.name)
                    .collect(),
            };
            if names.is_empty() {
                println!(
                    "no marketplaces are added; `ganja plugin marketplace add` is how one \
                     arrives"
                );
                return Ok(());
            }
            for name in names {
                let origin = store.update_marketplace(&name)?;
                println!("updated marketplace {name} from {origin}");
            }
            Ok(())
        }
        PluginAction::Install { spec } => {
            let Some((plugin, marketplace)) = spec.split_once('@') else {
                bail!(
                    "spell it <plugin>@<marketplace>, the way `ganja plugin list` and the \
                     marketplace file spell it; got \"{spec}\""
                );
            };
            store.install(plugin, marketplace)?;
            println!("installed {plugin} from {marketplace}, enabled");
            Ok(())
        }
        PluginAction::Enable { plugin } => {
            store.set_enabled(&plugin, true)?;
            println!("enabled {plugin}");
            Ok(())
        }
        PluginAction::Disable { plugin } => {
            store.set_enabled(&plugin, false)?;
            println!("disabled {plugin}");
            Ok(())
        }
        PluginAction::Remove { plugin } => {
            store.remove(&plugin)?;
            println!("removed {plugin}");
            Ok(())
        }
        PluginAction::Details { plugin } => details(&store, &plugin),
    }
}

/// Prints one plugin in Claude Code's own `details` shape: the header, the
/// component inventory, and the projected token costs with their own
/// disclaimers.
fn details(store: &Store, plugin: &str) -> Result<()> {
    let details = store.details(plugin)?;

    let version = details
        .version
        .as_deref()
        .map(|version| format!(" {version}"))
        .unwrap_or_default();
    let state = if details.enabled { "" } else { " (disabled)" };
    println!("{}{version}{state}", details.name);
    if let Some(description) = &details.description {
        println!("{}{description}", crate::INDENT);
    }
    println!(
        "{}Source: {}@{}",
        crate::INDENT,
        details.name,
        details.marketplace
    );

    println!();
    println!("Component inventory");
    let named = |label: &str, names: &[String]| {
        if names.is_empty() {
            println!("{}{label} (0)", crate::INDENT);
        } else {
            println!(
                "{}{label} ({})  {}",
                crate::INDENT,
                names.len(),
                names.join(", ")
            );
        }
    };
    let costed_names = |components: &[ganja_core::plugin::ComponentCost]| {
        components
            .iter()
            .map(|component| component.name.clone())
            .collect::<Vec<_>>()
    };
    named("Skills", &costed_names(&details.skills));
    named("Agents", &costed_names(&details.agents));
    named("Commands", &costed_names(&details.commands));
    named("Hooks", &details.hooks);
    named("MCP servers", &details.mcp);
    named("LSP servers", &details.lsp);

    println!();
    println!("Projected token cost");
    println!(
        "{}Always-on:   ~{} tok   added to every session",
        crate::INDENT,
        grouped(details.always_on_total())
    );

    let mut components: Vec<&ganja_core::plugin::ComponentCost> = Vec::new();
    components.extend(&details.skills);
    components.extend(&details.agents);
    components.extend(&details.commands);
    if !components.is_empty() {
        println!();
        println!("Per-component (rounded)");
        let width = components
            .iter()
            .map(|component| component.name.chars().count())
            .max()
            .unwrap_or(0)
            .max("component".len());
        println!(
            "{}{:<width$}  {:>9}  {:>9}",
            crate::INDENT,
            "component",
            "always-on",
            "on-invoke"
        );
        for component in components {
            println!(
                "{}{:<width$}  {:>9}  {:>9}",
                crate::INDENT,
                component.name,
                approx(component.always_on),
                approx(component.on_invoke)
            );
        }
        println!();
        println!(
            "{}On-invoke cost is paid each time a skill or agent fires.",
            crate::INDENT
        );
        println!(
            "{}Token counts are estimates and may differ from actual usage.",
            crate::INDENT
        );
    }

    Ok(())
}

/// A token estimate the way the reference output rounds one: `< 20` below
/// twenty, `~N` to the nearest ten below a thousand, `~N.Nk` above with a
/// clean `.0` dropped.
fn approx(tokens: u64) -> String {
    if tokens < 20 {
        "< 20".to_owned()
    } else if tokens < 1_000 {
        format!("~{}", (tokens + 5) / 10 * 10)
    } else {
        let tenths = (tokens + 50) / 100;
        if tenths.is_multiple_of(10) {
            format!("~{}k", tenths / 10)
        } else {
            format!("~{}.{}k", tenths / 10, tenths % 10)
        }
    }
}

/// `1620` as `1,620` — the always-on total is a real figure, grouped rather
/// than rounded.
fn grouped(tokens: u64) -> String {
    let digits = tokens.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }

    out
}

/// Prints every added marketplace: origin, offer count — or why its own
/// file no longer reads — and the installed plugins hung under it the way
/// `list` hangs components.
fn marketplaces(store: &Store) -> Result<()> {
    let listings = store.marketplaces()?;
    if listings.is_empty() {
        println!("no marketplaces are added; `ganja plugin marketplace add` is how one arrives");
        return Ok(());
    }

    for listing in listings {
        match &listing.offered {
            Ok(offered) => println!(
                "{} (from {}, offers {} plugin{})",
                listing.name,
                listing.origin,
                offered.len(),
                if offered.len() == 1 { "" } else { "s" },
            ),
            Err(reason) => println!(
                "{} (from {}, unreadable: {reason})",
                listing.name, listing.origin
            ),
        }
        for plugin in listing.installed {
            println!("{}installed: {plugin}", crate::INDENT);
        }
    }

    Ok(())
}

/// Prints every installed plugin, its state, where it came from, and what it
/// contributes — one plugin per stanza, components indented under it the way
/// the MCP listing hangs its tools.
fn list(store: &Store) -> Result<()> {
    let listings = store.list()?;
    if listings.is_empty() {
        println!(
            "no plugins are installed; `ganja plugin marketplace add` and \
             `ganja plugin install` are how one arrives"
        );
        return Ok(());
    }

    for listing in listings {
        let state = if listing.enabled {
            "enabled"
        } else {
            "disabled"
        };
        println!("{} ({state}, from {})", listing.name, listing.marketplace);
        if listing.components.is_empty() {
            println!("{}(no components)", crate::INDENT);
        }
        for component in listing.components {
            println!("{}{component}", crate::INDENT);
        }
    }

    Ok(())
}

#[cfg(test)]
#[path = "plugin_tests.rs"]
mod tests;
