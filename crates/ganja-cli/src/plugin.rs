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
    }
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
