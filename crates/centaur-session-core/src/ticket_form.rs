//! The versioned ticket intake contract. Clients supply data; Slack owns the layout.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const TICKET_FORM_PREFIX: &str = "centaur:ticket-form:v1:";
pub const MAX_FORM_BYTES: usize = 128 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TicketForm {
    pub title: String,
    pub summary: String,
    pub team: String,
    pub project: Option<String>,
    pub due: Option<String>,
    /// Complete eligible projects by team, including teams with no eligible projects.
    pub teams: BTreeMap<String, Vec<Project>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub id: String,
    pub name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TicketAnswer {
    team: String,
    #[serde(deserialize_with = "Option::deserialize")]
    project: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    due: Option<String>,
    context: String,
}

fn valid_date(value: &str) -> bool {
    let b = value.as_bytes();
    if b.len() != 10
        || !b.iter().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                *c == b'-'
            } else {
                c.is_ascii_digit()
            }
        })
    {
        return false;
    }
    let year = value[..4].parse::<i32>().unwrap();
    let month = value[5..7].parse::<u8>().unwrap();
    let day = value[8..].parse::<u8>().unwrap();
    year > 0
        && time::Month::try_from(month)
            .and_then(|m| time::Date::from_calendar_date(year, m, day))
            .is_ok()
}

impl TicketForm {
    fn valid_selection(&self, team: &str, project: Option<&str>, due: Option<&str>) -> bool {
        self.teams
            .get(team)
            .is_some_and(|choices| project.is_none_or(|id| choices.iter().any(|p| p.id == id)))
            && due.is_none_or(valid_date)
    }

    pub fn valid(&self) -> bool {
        !self.title.trim().is_empty()
            && self.title.trim() == self.title
            && !self.title.contains(['\r', '\n'])
            && self.title.chars().count() <= 130
            && !self.summary.trim().is_empty()
            && self.summary.chars().count() <= 3000
            && self.teams.len() <= 100
            && self.valid_selection(&self.team, self.project.as_deref(), self.due.as_deref())
            && self.teams.iter().all(|(team, projects)| {
                let mut ids = BTreeSet::new();
                !team.is_empty()
                    && team.len() <= 75
                    && team
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
                    && projects.len() < 10_000
                    && projects.iter().all(|p| {
                        !p.id.is_empty()
                            && p.id != "none"
                            && p.id.len() <= 75
                            && ids.insert(&p.id)
                            && !p.name.trim().is_empty()
                    })
            })
            && serde_json::to_vec(self).is_ok_and(|data| data.len() <= MAX_FORM_BYTES)
    }

    pub fn valid_answer(&self, value: &str) -> bool {
        serde_json::from_str::<TicketAnswer>(value).is_ok_and(|a| {
            self.valid_selection(&a.team, a.project.as_deref(), a.due.as_deref())
                && a.context.chars().count() <= 2000
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reject_cross_team_projects_invalid_dates_and_extra_fields() {
        let form: TicketForm = serde_json::from_value(json!({
            "title":"Fix loading", "summary":"The first page should open.",
            "team":"ENG","project":"p","due":null,
            "teams":{"ENG":[{"id":"p","name":"Client"}],"OPS":[]}
        }))
        .unwrap();
        assert!(form.valid());
        let answer = json!({"team":"ENG","project":"p","due":"2028-02-29","context":"Details"});
        assert!(form.valid_answer(&answer.to_string()));
        for (key, value) in [
            ("team", "OPS"),
            ("project", "deleted"),
            ("due", "2027-02-29"),
            ("scope", "injected"),
        ] {
            let mut invalid = answer.clone();
            invalid[key] = json!(value);
            assert!(!form.valid_answer(&invalid.to_string()));
        }
        let mut incomplete = answer.clone();
        incomplete.as_object_mut().unwrap().remove("project");
        assert!(!form.valid_answer(&incomplete.to_string()));
        let none = json!({"team":"OPS","project":null,"due":null,"context":""});
        assert!(form.valid_answer(&none.to_string()));
    }
}
