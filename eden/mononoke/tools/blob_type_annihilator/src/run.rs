/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Opening a run: its state directory, its re-resolved plan and its stores.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use clap::Args;
use mononoke_app::MononokeApp;

use crate::adapters::AdapterOptions;
use crate::adapters::ComponentStore;
use crate::progress::ProgressOptions;
use crate::scan;
use crate::scan::Cell;
use crate::scope;
use crate::scope::Plan;
use crate::scope::PlannedComponent;
use crate::scope::ResolvedPlan;
use crate::state::StateDir;

/// What every command working on a recorded plan takes.
#[derive(Args, Clone, Debug)]
pub struct RunArgs {
    /// Directory holding this run's plan, inventory and journals
    #[clap(long)]
    pub state_dir: PathBuf,

    #[clap(flatten)]
    pub adapter: AdapterOptions,

    #[clap(flatten)]
    pub progress: ProgressOptions,
}

/// The opened stores of a run, by component id.
pub struct Stores(BTreeMap<String, ComponentStore>);

impl Stores {
    pub fn get(&self, component_id: &str) -> Result<&ComponentStore> {
        self.0
            .get(component_id)
            .with_context(|| format!("component {component_id} is not open"))
    }

    pub fn contains(&self, component_id: &str) -> bool {
        self.0.contains_key(component_id)
    }
}

/// An opened state directory whose plan still matches the configuration.
pub struct Run {
    pub state: StateDir,
    pub resolved: ResolvedPlan,
}

impl Run {
    pub async fn open(app: &MononokeApp, state_dir: &Path) -> Result<Self> {
        let (state, plan) = StateDir::open(state_dir).await?;
        let resolved = scope::resolve(app, plan.request.clone())?;
        plan.ensure_unchanged(&resolved.plan)?;
        Ok(Self { state, resolved })
    }

    /// Opens a run and the stores of its supported components, restricted
    /// to `only` if it is not empty.
    pub async fn open_with_stores(
        app: &MononokeApp,
        args: &RunArgs,
        only: &[String],
    ) -> Result<(Self, Stores)> {
        let run = Self::open(app, &args.state_dir).await?;
        let stores = run
            .open_stores(app, &run.components(only)?, &args.adapter)
            .await?;
        Ok((run, stores))
    }

    pub fn plan(&self) -> &Plan {
        &self.resolved.plan
    }

    /// The plan's supported components, restricted to `only` if it is not
    /// empty.
    pub fn components(&self, only: &[String]) -> Result<Vec<&PlannedComponent>> {
        for id in only {
            if !self.plan().component(id)?.is_supported() {
                bail!("component {id} is not supported by this tool");
            }
        }
        Ok(self
            .plan()
            .components
            .iter()
            .filter(|component| component.is_supported())
            .filter(|component| only.is_empty() || only.contains(&component.id))
            .collect())
    }

    pub async fn open_stores(
        &self,
        app: &MononokeApp,
        components: &[&PlannedComponent],
        options: &AdapterOptions,
    ) -> Result<Stores> {
        let mut stores = BTreeMap::new();
        for component in components {
            let config = self
                .resolved
                .blob_configs
                .get(&component.id)
                .with_context(|| format!("no config for component {}", component.id))?;
            let store = ComponentStore::open(app, config, options)
                .await
                .with_context(|| format!("opening component {}", component.id))?;
            stores.insert(component.id.clone(), store);
        }
        Ok(Stores(stores))
    }

    /// The plan's cells in the components of `stores`.
    pub fn cells(&self, stores: &Stores) -> Vec<Cell<'_>> {
        scan::cells(self.plan())
            .filter(|cell| stores.contains(&cell.component.id))
            .collect()
    }
}
