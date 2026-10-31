use crate::context::{self, ContextRequest, ContextResult, ReadRange, Snippet};
use crate::resident::Resident;
use crate::search::{self, Search, SearchResult};
use crate::workspace::Workspace;
use crate::{Result, invalid};
use clap::ValueEnum;
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Backend {
    Shared,
    Live,
    Ram,
    Index,
}

pub struct Engine {
    pub(crate) workspace: Workspace,
    pub(crate) backend: Backend,
    resident: Option<Resident>,
}

impl From<Workspace> for Engine {
    fn from(workspace: Workspace) -> Self {
        Self {
            workspace,
            backend: Backend::Live,
            resident: None,
        }
    }
}

impl Engine {
    pub fn new(workspace: Workspace, backend: Backend) -> Result<Self> {
        let resident = match backend {
            Backend::Live => None,
            Backend::Shared => {
                return Err(invalid("shared backend requires the daemon adapter").into());
            }
            _ => Some(Resident::load(&workspace, backend == Backend::Index, 1)?),
        };
        Ok(Self {
            workspace,
            backend,
            resident,
        })
    }

    pub fn search(&self, request: &Search) -> Result<SearchResult> {
        match &self.resident {
            Some(resident) => resident.search(request),
            None => search::search(&self.workspace, request),
        }
    }

    pub fn read_many(&self, ranges: &[ReadRange]) -> Result<Vec<Snippet>> {
        match &self.resident {
            Some(resident) => resident.read_many(ranges),
            None => context::read_many(&self.workspace, ranges),
        }
    }

    pub fn files(&self, request: &crate::files::FileQuery) -> Result<crate::files::FileResult> {
        match &self.resident {
            Some(resident) => {
                crate::files::find(resident.paths.keys().map(|p| p.as_ref()), request)
            }
            None => {
                let files = self.workspace.files()?;
                let names: Vec<_> = files.iter().map(|p| p.to_string_lossy()).collect();
                crate::files::find(names.iter().map(|p| p.as_ref()), request)
            }
        }
    }

    pub fn context(&self, request: &ContextRequest) -> Result<ContextResult> {
        context::validate_context(request)?;
        if self.resident.is_some() {
            context::assemble_trusted(self.search(&request.search)?, request.radius, |ranges| {
                self.read_many(ranges)
            })
        } else {
            context::assemble(self.search(&request.search)?, request.radius, |ranges| {
                self.read_many(ranges)
            })
        }
    }

    pub fn status(&self) -> Value {
        match &self.resident {
            Some(resident) => json!({"backend":format!("{:?}",self.backend),"stats":resident.stats,
                "freshness":"explicit snapshot; call code_refresh after edits or ignore/access-policy changes; not safe for unattended live editing"}),
            None => json!({"backend":"Live"}),
        }
    }

    pub(crate) fn candidate_work(&self, pattern: &str) -> Value {
        match &self.resident {
            Some(resident) => {
                let (files, bytes) = resident.candidate_stats(pattern);
                json!({"files":files,"bytes":bytes})
            }
            None => Value::Null,
        }
    }

    pub fn refresh(&mut self) -> Result<Value> {
        let generation = self
            .resident
            .as_ref()
            .ok_or_else(|| invalid("live backend needs no refresh"))?
            .stats
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("generation overflow"))?;
        let replacement =
            Resident::load(&self.workspace, self.backend == Backend::Index, generation)?;
        self.resident = Some(replacement);
        Ok(self.status())
    }
}
