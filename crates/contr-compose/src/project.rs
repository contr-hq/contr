use std::collections::BTreeMap;
use std::path::Path;

use contr_api::compose::v1::{Project, Service};
use contr_api::containers::v1::Container;
use serde_json::Value;

/// Labels Compose sets on the containers it creates.
pub mod labels {
    /// Project name.
    pub const PROJECT: &str = "com.docker.compose.project";
    /// Service name.
    pub const SERVICE: &str = "com.docker.compose.service";
    /// Directory the project's relative paths start from.
    pub const WORKING_DIR: &str = "com.docker.compose.project.working_dir";
    /// Compose files, separated by commas.
    pub const CONFIG_FILES: &str = "com.docker.compose.project.config_files";
    /// `True` on containers of `docker compose run`, which are not part of the service.
    pub const ONE_OFF: &str = "com.docker.compose.oneoff";
    /// Replica number, from 1.
    pub const NUMBER: &str = "com.docker.compose.container-number";
}

/// What a project's Compose files declare.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Declared {
    pub name: String,
    pub working_dir: String,
    pub services: BTreeMap<String, DeclaredService>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeclaredService {
    pub image: String,
    pub build_context: String,
}

impl Declared {
    /// What `docker compose config --format json` printed for `config_files`, whose first file's directory is the
    /// working directory.
    pub fn parse(json: &str, config_files: &[String]) -> Result<Self, serde_json::Error> {
        let config: Value = serde_json::from_str(json)?;
        let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
        let services = config["services"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(name, service)| {
                let build_context = match &service["build"] {
                    Value::String(context) => context.clone(),
                    build => text(&build["context"]),
                };
                (
                    name.clone(),
                    DeclaredService {
                        image: text(&service["image"]),
                        build_context,
                    },
                )
            })
            .collect();

        Ok(Self {
            name: text(&config["name"]),
            working_dir: config_files.first().map(|file| parent(file)).unwrap_or_default(),
            services,
        })
    }
}

/// A project the user added, with what Compose read from its files.
#[derive(Clone, Copy, Debug)]
pub struct Added<'a> {
    pub config_files: &'a [String],
    pub declared: &'a Result<Declared, String>,
}

/// Compose files a container records for its project.
pub fn config_files(container: &Container) -> Vec<String> {
    container
        .labels
        .get(labels::CONFIG_FILES)
        .map(|files| {
            files
                .split(',')
                .filter(|file| !file.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Name of a project the user added by its `config_files`: the one Compose read from them, else the one Compose would
/// give it.
pub fn added_name(config_files: &[String], declared: &Result<Declared, String>) -> String {
    match declared {
        Ok(declared) if !declared.name.is_empty() => declared.name.clone(),
        _ => default_name(config_files),
    }
}

/// Projects of `containers` and the `added` ones, sorted by name; `declared` returns what Compose read from a set of
/// Compose files, when it read them.
pub fn group(
    containers: &[Container],
    added: &[Added<'_>],
    declared: impl Fn(&[String]) -> Option<Result<Declared, String>>,
) -> Vec<Project> {
    let mut projects: BTreeMap<String, Building> = BTreeMap::new();
    for container in containers {
        let label = |key: &str| container.labels.get(key).cloned().unwrap_or_default();
        let name = label(labels::PROJECT);
        if name.is_empty() || label(labels::ONE_OFF).eq_ignore_ascii_case("true") {
            continue;
        }
        let project = projects.entry(name.clone()).or_insert_with(|| Building::named(name));
        if project.project.working_dir.is_empty() {
            project.project.working_dir = label(labels::WORKING_DIR);
        }
        if project.project.config_files.is_empty() {
            project.project.config_files = config_files(container);
        }
        project
            .services
            .entry(label(labels::SERVICE))
            .or_default()
            .push(container.clone());
    }

    for added in added {
        let name = added_name(added.config_files, added.declared);
        let project = projects.entry(name.clone()).or_insert_with(|| Building::named(name));
        project.project.added = true;
        if project.project.config_files.is_empty() {
            project.project.config_files = added.config_files.to_vec();
        }
        if let Err(error) = added.declared {
            project.project.config_error.clone_from(error);
        }
    }

    projects
        .into_values()
        .map(|building| {
            let read = Some(&building.project.config_files)
                .filter(|files| !files.is_empty())
                .and_then(|files| declared(files));
            building.finish(read)
        })
        .collect()
}

/// A project while its containers are gathered.
struct Building {
    project: Project,
    services: BTreeMap<String, Vec<Container>>,
}

impl Building {
    fn named(name: String) -> Self {
        Self {
            project: Project {
                name,
                ..Project::default()
            },
            services: BTreeMap::new(),
        }
    }

    fn finish(self, read: Option<Result<Declared, String>>) -> Project {
        let Self {
            mut project,
            mut services,
        } = self;
        let declared = match read {
            Some(Ok(declared)) => {
                if project.working_dir.is_empty() {
                    project.working_dir.clone_from(&declared.working_dir);
                }
                declared.services
            }
            Some(Err(error)) => {
                project.config_error = error;
                BTreeMap::new()
            }
            None => BTreeMap::new(),
        };
        for name in declared.keys() {
            services.entry(name.clone()).or_default();
        }

        project.services = services
            .into_iter()
            .map(|(name, mut containers)| {
                containers.sort_by_key(replica);
                let declared = declared.get(&name).cloned().unwrap_or_default();
                let newest = containers
                    .iter()
                    .max_by_key(|container| container.created_at.map(|created| created.seconds));
                Service {
                    image: newest.map_or(declared.image, |container| container.image.clone()),
                    build_context: declared.build_context,
                    name,
                    containers,
                }
            })
            .collect();
        project
    }
}

fn replica(container: &Container) -> u32 {
    container
        .labels
        .get(labels::NUMBER)
        .and_then(|number| number.parse().ok())
        .unwrap_or(0)
}

/// The name Compose gives a project that names none: its directory's name, lowercased, keeping letters, digits, `-`
/// and `_`.
fn default_name(config_files: &[String]) -> String {
    let directory = config_files.first().map(|file| parent(file)).unwrap_or_default();
    let base = directory
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or_default();

    base.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

/// Directory of `file`, a path on either Windows or Linux.
fn parent(file: &str) -> String {
    match file.rfind(['/', '\\']) {
        Some(0) => file[..1].to_owned(),
        Some(end) => file[..end].to_owned(),
        None => Path::new(file)
            .parent()
            .map(|parent| parent.display().to_string())
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use contr_api::containers::v1::ContainerState;
    use prost_types::Timestamp;

    use super::*;

    fn container(id: &str, project: &str, service: &str, number: u32, created: i64) -> Container {
        Container {
            id: id.to_owned(),
            image: format!("{service}:{created}"),
            created_at: Some(Timestamp {
                seconds: created,
                nanos: 0,
            }),
            state: ContainerState::Running.into(),
            labels: HashMap::from([
                (labels::PROJECT.to_owned(), project.to_owned()),
                (labels::SERVICE.to_owned(), service.to_owned()),
                (labels::NUMBER.to_owned(), number.to_string()),
                (labels::WORKING_DIR.to_owned(), r"C:\src\shop".to_owned()),
                (
                    labels::CONFIG_FILES.to_owned(),
                    r"C:\src\shop\compose.yaml,C:\src\shop\compose.override.yaml".to_owned(),
                ),
            ]),
            ..Container::default()
        }
    }

    fn shop() -> Declared {
        Declared {
            name: "shop".to_owned(),
            working_dir: r"C:\src\shop".to_owned(),
            services: BTreeMap::from([
                (
                    "api".to_owned(),
                    DeclaredService {
                        image: "node:22".to_owned(),
                        build_context: String::new(),
                    },
                ),
                (
                    "worker".to_owned(),
                    DeclaredService {
                        image: String::new(),
                        build_context: r"C:\src\shop\worker".to_owned(),
                    },
                ),
            ]),
        }
    }

    #[test]
    fn groups_containers_by_project_and_service() {
        let mut one_off = container("run", "shop", "api", 1, 9);
        one_off.labels.insert(labels::ONE_OFF.to_owned(), "True".to_owned());
        let containers = [
            container("api2", "shop", "api", 2, 5),
            container("api1", "shop", "api", 1, 7),
            container("docs", "docs", "web", 1, 3),
            one_off,
            Container {
                id: "plain".to_owned(),
                ..Container::default()
            },
        ];

        let projects = group(&containers, &[], |files| (files.len() == 2).then(|| Ok(shop())));

        let names: Vec<&str> = projects.iter().map(|project| project.name.as_str()).collect();
        assert_eq!(names, ["docs", "shop"]);
        let shop = &projects[1];
        assert_eq!(shop.config_files.len(), 2);
        let services: Vec<(&str, Vec<&str>, &str, &str)> = shop
            .services
            .iter()
            .map(|service| {
                (
                    service.name.as_str(),
                    service
                        .containers
                        .iter()
                        .map(|container| container.id.as_str())
                        .collect(),
                    service.image.as_str(),
                    service.build_context.as_str(),
                )
            })
            .collect();
        assert_eq!(
            services,
            [
                ("api", vec!["api1", "api2"], "api:7", ""),
                ("worker", vec![], "", r"C:\src\shop\worker"),
            ]
        );
    }

    #[test]
    fn added_projects_stay_listed_without_containers() {
        let files = [r"C:\src\Payments POC\compose.yaml".to_owned()];
        let unreadable = Err("no such file".to_owned());
        let readable = Ok(shop());

        let projects = group(
            &[container("api1", "shop", "api", 1, 1)],
            &[
                Added {
                    config_files: &files,
                    declared: &unreadable,
                },
                Added {
                    config_files: &files,
                    declared: &readable,
                },
            ],
            |_| None,
        );

        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].name, "paymentspoc");
        assert!(projects[0].added);
        assert_eq!(projects[0].config_error, "no such file");
        assert_eq!(projects[0].config_files, files);
        assert_eq!(projects[1].name, "shop");
        assert!(projects[1].added, "a running project the user also added");
        assert_eq!(projects[1].services[0].containers.len(), 1);
    }

    #[test]
    fn reads_compose_config_output() {
        let json = r#"{"name": "shop", "services": {
            "api": {"image": "node:22", "ports": [{"target": 3000}]},
            "worker": {"build": {"context": "/src/shop/worker", "dockerfile": "Dockerfile"}},
            "short": {"build": "/src/shop/short"}
        }}"#;

        let declared = Declared::parse(json, &["/src/shop/compose.yaml".to_owned()]).unwrap();

        assert_eq!(declared.name, "shop");
        assert_eq!(declared.working_dir, "/src/shop");
        assert_eq!(declared.services["api"].image, "node:22");
        assert_eq!(declared.services["worker"].build_context, "/src/shop/worker");
        assert_eq!(declared.services["short"].build_context, "/src/shop/short");
    }

    #[test]
    fn finds_parents_of_windows_and_linux_paths() {
        assert_eq!(parent(r"C:\src\shop\compose.yaml"), r"C:\src\shop");
        assert_eq!(parent("/src/shop/compose.yaml"), "/src/shop");
        assert_eq!(parent("/compose.yaml"), "/");
        assert_eq!(default_name(&["/home/me/My_App-2/compose.yaml".to_owned()]), "my_app-2");
    }
}
