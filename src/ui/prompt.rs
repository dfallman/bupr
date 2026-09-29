//! Interactive answers to the §6 step-5 questions, via inquire.

use std::path::PathBuf;

use inquire::{Confirm, Select};

use crate::config::Preset;
use crate::engine::PlanSummary;
use crate::preflight::MarkerStatus;
use crate::runner::{DeleteChoice, Prompter};
use crate::ui::format::{bytes, count, tilde};

pub struct InquirePrompter {
    pub home: PathBuf,
}

impl Prompter for InquirePrompter {
    fn adopt(&mut self, preset: &Preset, s: &PlanSummary) -> bool {
        let dest = tilde(&s.dest, &self.home);
        match &s.marker {
            MarkerStatus::Foreign => {
                println!("{dest} already contains files that bupr did not put there.")
            }
            MarkerStatus::Mismatch {
                preset: other,
                source,
            } => {
                println!(
                    "{dest} is the backup folder of preset \"{other}\" ({}).",
                    tilde(source, &self.home)
                )
            }
            _ => return true,
        }
        println!(
            "Mirroring \"{}\" into it deletes everything there that is not in {}.",
            preset.name,
            tilde(&s.source, &self.home)
        );
        Confirm::new("Use this folder for this preset anyway?")
            .with_default(false)
            .prompt()
            .unwrap_or(false)
    }

    fn deletions(&mut self, preset: &Preset, s: &PlanSummary) -> DeleteChoice {
        println!(
            "This run would delete {} entries ({}) from {} — above the limit of {} / {}.",
            count(s.totals.delete_entries),
            bytes(s.totals.delete_bytes),
            tilde(&s.dest, &self.home),
            count(preset.max_delete),
            bytes(preset.max_delete_bytes)
        );
        for (p, size) in &s.largest_deletes {
            println!("    - {p}  ({})", bytes(*size));
        }
        let options = vec!["Delete them", "Skip deletions this run", "Abort"];
        match Select::new("What should bupr do?", options).prompt() {
            Ok("Delete them") => DeleteChoice::Delete,
            Ok("Skip deletions this run") => DeleteChoice::Skip,
            _ => DeleteChoice::Abort,
        }
    }

    fn low_space(&mut self, _: &Preset, s: &PlanSummary) -> bool {
        println!(
            "Only {} free on the destination, but {} are needed.",
            bytes(s.free_bytes.unwrap_or(0)),
            bytes(s.needed_bytes)
        );
        Confirm::new("Continue anyway?")
            .with_default(false)
            .prompt()
            .unwrap_or(false)
    }

    fn secrets(&mut self, preset: &Preset, s: &PlanSummary) -> bool {
        println!(
            "{} secret file(s) such as .env or keys would be copied to {}, which is not known to be encrypted.",
            count(s.secret_files),
            tilde(&s.dest, &self.home)
        );
        println!(
            "Preset \"{}\" sets secrets_require_encryption.",
            preset.name
        );
        Confirm::new("Copy them anyway?")
            .with_default(false)
            .prompt()
            .unwrap_or(false)
    }
}
