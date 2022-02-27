use anyhow::{bail, Context, Result};
use clap::{App, AppSettings, Arg, ArgMatches};
use control_plane::compute::ComputeControlPlane;
use control_plane::local_env;
use control_plane::local_env::LocalEnv;
use control_plane::safekeeper::SafekeeperNode;
use control_plane::storage::PageServerNode;
use pageserver::config::defaults::{
    DEFAULT_HTTP_LISTEN_ADDR as DEFAULT_PAGESERVER_HTTP_ADDR,
    DEFAULT_PG_LISTEN_ADDR as DEFAULT_PAGESERVER_PG_ADDR,
};
use std::collections::HashMap;
use std::process::exit;
use std::str::FromStr;
use walkeeper::defaults::{
    DEFAULT_HTTP_LISTEN_PORT as DEFAULT_SAFEKEEPER_HTTP_PORT,
    DEFAULT_PG_LISTEN_PORT as DEFAULT_SAFEKEEPER_PG_PORT,
};
use zenith_utils::auth::{Claims, Scope};
use zenith_utils::postgres_backend::AuthType;
use zenith_utils::zid::{ZTenantId, ZTimelineId};
use zenith_utils::GIT_VERSION;

use pageserver::branches::BranchInfo;

// Default name of a safekeeper node, if not specified on the command line.
const DEFAULT_SAFEKEEPER_NAME: &str = "single";

fn default_conf() -> String {
    format!(
        r#"
# Default built-in configuration, defined in main.rs
[pageserver]
listen_pg_addr = '{pageserver_pg_addr}'
listen_http_addr = '{pageserver_http_addr}'
auth_type = '{pageserver_auth_type}'

[[safekeepers]]
name = '{safekeeper_name}'
pg_port = {safekeeper_pg_port}
http_port = {safekeeper_http_port}
"#,
        pageserver_pg_addr = DEFAULT_PAGESERVER_PG_ADDR,
        pageserver_http_addr = DEFAULT_PAGESERVER_HTTP_ADDR,
        pageserver_auth_type = AuthType::Trust,
        safekeeper_name = DEFAULT_SAFEKEEPER_NAME,
        safekeeper_pg_port = DEFAULT_SAFEKEEPER_PG_PORT,
        safekeeper_http_port = DEFAULT_SAFEKEEPER_HTTP_PORT,
    )
}

///
/// Branches tree element used as a value in the HashMap.
///
struct BranchTreeEl {
    /// `BranchInfo` received from the `pageserver` via the `branch_list` libpq API call.
    pub info: BranchInfo,
    /// Holds all direct children of this branch referenced using `timeline_id`.
    pub children: Vec<String>,
}

// Main entry point for the 'zenith' CLI utility
//
// This utility helps to manage zenith installation. That includes following:
//   * Management of local postgres installations running on top of the
//     pageserver.
//   * Providing CLI api to the pageserver
//   * TODO: export/import to/from usual postgres
fn main() -> Result<()> {
    #[rustfmt::skip] // rustfmt squashes these into a single line otherwise
    let pg_node_arg = Arg::new("node")
        .index(1)
        .help("Node name")
        .required(true);

    #[rustfmt::skip]
    let safekeeper_node_arg = Arg::new("node")
        .index(1)
        .help("Node name")
        .required(false);

    let timeline_arg = Arg::new("timeline")
        .index(2)
        .help("Branch name or a point-in time specification")
        .required(false);

    let tenantid_arg = Arg::new("tenantid")
        .long("tenantid")
        .help("Tenant id. Represented as a hexadecimal string 32 symbols length")
        .takes_value(true)
        .required(false);

    let port_arg = Arg::new("port")
        .long("port")
        .required(false)
        .value_name("port");

    let stop_mode_arg = Arg::new("stop-mode")
        .short('m')
        .takes_value(true)
        .possible_values(&["fast", "immediate"])
        .help("If 'immediate', don't flush repository data at shutdown")
        .required(false)
        .value_name("stop-mode");

    let pageserver_config_args = Arg::new("pageserver-config-override")
        .long("pageserver-config-override")
        .takes_value(true)
        .number_of_values(1)
        .multiple_occurrences(true)
        .help("Additional pageserver's configuration options or overrides, refer to pageserver's 'config-override' CLI parameter docs for more")
        .required(false);

    let matches = App::new("Zenith CLI")
        .setting(AppSettings::ArgRequiredElseHelp)
        .version(GIT_VERSION)
        .subcommand(
            App::new("init")
                .about("Initialize a new Zenith repository")
                .arg(pageserver_config_args.clone())
                .arg(
                    Arg::new("config")
                        .long("config")
                        .required(false)
                        .value_name("config"),
                )
        )
        .subcommand(
            App::new("branch")
                .about("Create a new branch")
                .arg(Arg::new("branchname").required(false).index(1))
                .arg(Arg::new("start-point").required(false).index(2))
                .arg(tenantid_arg.clone()),
        ).subcommand(
            App::new("tenant")
            .setting(AppSettings::ArgRequiredElseHelp)
            .about("Manage tenants")
            .subcommand(App::new("list"))
            .subcommand(App::new("create").arg(Arg::new("tenantid").required(false).index(1)))
        )
        .subcommand(
            App::new("pageserver")
                .setting(AppSettings::ArgRequiredElseHelp)
                .about("Manage pageserver")
                .subcommand(App::new("status"))
                .subcommand(App::new("start").about("Start local pageserver").arg(pageserver_config_args.clone()))
                .subcommand(App::new("stop").about("Stop local pageserver")
                            .arg(stop_mode_arg.clone()))
                .subcommand(App::new("restart").about("Restart local pageserver").arg(pageserver_config_args.clone()))
        )
        .subcommand(
            App::new("safekeeper")
                .setting(AppSettings::ArgRequiredElseHelp)
                .about("Manage safekeepers")
                .subcommand(App::new("start")
                            .about("Start local safekeeper")
                            .arg(safekeeper_node_arg.clone())
                )
                .subcommand(App::new("stop")
                            .about("Stop local safekeeper")
                            .arg(safekeeper_node_arg.clone())
                            .arg(stop_mode_arg.clone())
                )
                .subcommand(App::new("restart")
                            .about("Restart local safekeeper")
                            .arg(safekeeper_node_arg.clone())
                            .arg(stop_mode_arg.clone())
                )
        )
        .subcommand(
            App::new("pg")
                .setting(AppSettings::ArgRequiredElseHelp)
                .about("Manage postgres instances")
                .subcommand(App::new("list").arg(tenantid_arg.clone()))
                .subcommand(App::new("create")
                    .about("Create a postgres compute node")
                    .arg(pg_node_arg.clone())
                    .arg(timeline_arg.clone())
                    .arg(tenantid_arg.clone())
                    .arg(port_arg.clone())
                    .arg(
                        Arg::new("config-only")
                            .help("Don't do basebackup, create compute node with only config files")
                            .long("config-only")
                            .required(false)
                    ))
                .subcommand(App::new("start")
                    .about("Start a postgres compute node.\n This command actually creates new node from scratch, but preserves existing config files")
                    .arg(pg_node_arg.clone())
                    .arg(timeline_arg.clone())
                    .arg(tenantid_arg.clone())
                    .arg(port_arg.clone()))
                .subcommand(App::new("start-mr")
                    .about("Start a postgres compute node in multi-region mode.\n This command is similar to \"start\", but for the multi-region mode")
                    .arg(pg_node_arg.clone())
                    .arg(Arg::new("global")
                        .index(2)
                        .help("Global branch name")
                        .required(true))
                    .arg(Arg::new("regions")
                        .index(3)
                        .help("Comma-separated list of region specs with the form branch_names@ip:[port][*]. Index of a region spec in this list corresponds to its region id.\n\"*\" marks the current region and must appear exactly once.\n\"port\" is the safekeeper's port.")
                        .required(true))
                    .arg(tenantid_arg.clone())
                    .arg(port_arg.clone()))
                .subcommand(
                    App::new("stop")
                        .arg(pg_node_arg.clone())
                        .arg(timeline_arg.clone())
                        .arg(tenantid_arg.clone())
                        .arg(
                            Arg::new("destroy")
                                .help("Also delete data directory (now optional, should be default in future)")
                                .long("destroy")
                                .required(false)
                        )
                    )

        )
        .subcommand(
            App::new("start")
                .about("Start page server and safekeepers")
                .arg(pageserver_config_args)
        )
        .subcommand(
            App::new("stop")
                .about("Stop page server and safekeepers")
                .arg(stop_mode_arg.clone())
        )
        .get_matches();

    let (sub_name, sub_args) = match matches.subcommand() {
        Some(subcommand_data) => subcommand_data,
        None => bail!("no subcommand provided"),
    };

    // Check for 'zenith init' command first.
    let subcmd_result = if sub_name == "init" {
        handle_init(sub_args)
    } else {
        // all other commands need an existing config
        let env = match LocalEnv::load_config() {
            Ok(conf) => conf,
            Err(e) => {
                eprintln!("Error loading config: {}", e);
                exit(1);
            }
        };

        match sub_name {
            "tenant" => handle_tenant(sub_args, &env),
            "branch" => handle_branch(sub_args, &env),
            "start" => handle_start_all(sub_args, &env),
            "stop" => handle_stop_all(sub_args, &env),
            "pageserver" => handle_pageserver(sub_args, &env),
            "pg" => handle_pg(sub_args, &env),
            "safekeeper" => handle_safekeeper(sub_args, &env),
            _ => bail!("unexpected subcommand {}", sub_name),
        }
    };
    if let Err(e) = subcmd_result {
        eprintln!("command failed: {:#}", e);
        exit(1);
    }

    Ok(())
}

///
/// Prints branches list as a tree-like structure.
///
fn print_branches_tree(branches: Vec<BranchInfo>) -> Result<()> {
    let mut branches_hash: HashMap<String, BranchTreeEl> = HashMap::new();

    // Form a hash table of branch timeline_id -> BranchTreeEl.
    for branch in &branches {
        branches_hash.insert(
            branch.timeline_id.to_string(),
            BranchTreeEl {
                info: branch.clone(),
                children: Vec::new(),
            },
        );
    }

    // Memorize all direct children of each branch.
    for branch in &branches {
        if let Some(tid) = &branch.ancestor_id {
            branches_hash
                .get_mut(tid)
                .context("missing branch info in the HashMap")?
                .children
                .push(branch.timeline_id.to_string());
        }
    }

    // Sort children by tid to bring some minimal order.
    for branch in &mut branches_hash.values_mut() {
        branch.children.sort();
    }

    for branch in branches_hash.values() {
        // Start with root branches (no ancestors) first.
        // Now there is 'main' branch only, but things may change.
        if branch.info.ancestor_id.is_none() {
            print_branch(0, &Vec::from([true]), branch, &branches_hash)?;
        }
    }

    Ok(())
}

///
/// Recursively prints branch info with all its children.
///
fn print_branch(
    nesting_level: usize,
    is_last: &[bool],
    branch: &BranchTreeEl,
    branches: &HashMap<String, BranchTreeEl>,
) -> Result<()> {
    // Draw main padding
    print!(" ");

    if nesting_level > 0 {
        let lsn = branch
            .info
            .ancestor_lsn
            .as_ref()
            .context("missing branch info in the HashMap")?;
        let mut br_sym = "┣━";

        // Draw each nesting padding with proper style
        // depending on whether its branch ended or not.
        if nesting_level > 1 {
            for l in &is_last[1..is_last.len() - 1] {
                if *l {
                    print!("   ");
                } else {
                    print!("┃  ");
                }
            }
        }

        // We are the last in this sub-branch
        if *is_last.last().unwrap() {
            br_sym = "┗━";
        }

        print!("{} @{}: ", br_sym, lsn);
    }

    // Finally print a branch name with new line
    println!("{}", branch.info.name);

    let len = branch.children.len();
    let mut i: usize = 0;
    let mut is_last_new = Vec::from(is_last);
    is_last_new.push(false);

    for child in &branch.children {
        i += 1;

        // Mark that the last padding is the end of the branch
        if i == len {
            if let Some(last) = is_last_new.last_mut() {
                *last = true;
            }
        }

        print_branch(
            nesting_level + 1,
            &is_last_new,
            branches
                .get(child)
                .context("missing branch info in the HashMap")?,
            branches,
        )?;
    }

    Ok(())
}

/// Returns a map of timeline IDs to branch_name@lsn strings.
/// Connects to the pageserver to query this information.
fn get_branch_infos(
    env: &local_env::LocalEnv,
    tenantid: &ZTenantId,
) -> Result<HashMap<ZTimelineId, BranchInfo>> {
    let page_server = PageServerNode::from_env(env);
    let branch_infos: Vec<BranchInfo> = page_server.branch_list(tenantid)?;
    let branch_infos: HashMap<ZTimelineId, BranchInfo> = branch_infos
        .into_iter()
        .map(|branch_info| (branch_info.timeline_id, branch_info))
        .collect();

    Ok(branch_infos)
}

// Helper function to parse --tenantid option, or get the default from config file
fn get_tenantid(sub_match: &ArgMatches, env: &local_env::LocalEnv) -> Result<ZTenantId> {
    if let Some(tenantid_cmd) = sub_match.value_of("tenantid") {
        Ok(ZTenantId::from_str(tenantid_cmd)?)
    } else if let Some(tenantid_conf) = env.default_tenantid {
        Ok(tenantid_conf)
    } else {
        bail!("No tenantid. Use --tenantid, or set 'default_tenantid' in the config file");
    }
}

fn handle_init(init_match: &ArgMatches) -> Result<()> {
    // Create config file
    let toml_file: String = if let Some(config_path) = init_match.value_of("config") {
        // load and parse the file
        std::fs::read_to_string(std::path::Path::new(config_path))
            .with_context(|| format!("Could not read configuration file \"{}\"", config_path))?
    } else {
        // Built-in default config
        default_conf()
    };

    let mut env =
        LocalEnv::create_config(&toml_file).context("Failed to create zenith configuration")?;
    env.init()
        .context("Failed to initialize zenith repository")?;

    // Call 'pageserver init'.
    let pageserver = PageServerNode::from_env(&env);
    if let Err(e) = pageserver.init(
        // default_tenantid was generated by the `env.init()` call above
        Some(&env.default_tenantid.unwrap().to_string()),
        &pageserver_config_overrides(init_match),
    ) {
        eprintln!("pageserver init failed: {}", e);
        exit(1);
    }

    Ok(())
}

fn pageserver_config_overrides(init_match: &ArgMatches) -> Vec<&str> {
    init_match
        .values_of("pageserver-config-override")
        .into_iter()
        .flatten()
        .collect()
}

fn handle_tenant(tenant_match: &ArgMatches, env: &local_env::LocalEnv) -> Result<()> {
    let pageserver = PageServerNode::from_env(env);
    match tenant_match.subcommand() {
        Some(("list", _)) => {
            for t in pageserver.tenant_list()? {
                println!("{} {}", t.id, t.state);
            }
        }
        Some(("create", create_match)) => {
            let tenantid = match create_match.value_of("tenantid") {
                Some(tenantid) => ZTenantId::from_str(tenantid)?,
                None => ZTenantId::generate(),
            };
            println!("using tenant id {}", tenantid);
            pageserver.tenant_create(tenantid)?;
            println!("tenant successfully created on the pageserver");
        }
        Some((sub_name, _)) => bail!("Unexpected tenant subcommand '{}'", sub_name),
        None => bail!("no tenant subcommand provided"),
    }
    Ok(())
}

fn handle_branch(branch_match: &ArgMatches, env: &local_env::LocalEnv) -> Result<()> {
    let pageserver = PageServerNode::from_env(env);

    let tenantid = get_tenantid(branch_match, env)?;

    if let Some(branchname) = branch_match.value_of("branchname") {
        let startpoint_str = branch_match
            .value_of("start-point")
            .context("Missing start-point")?;
        let branch = pageserver.branch_create(branchname, startpoint_str, &tenantid)?;
        println!(
            "Created branch '{}' at {:?} for tenant: {}",
            branch.name, branch.latest_valid_lsn, tenantid,
        );
    } else {
        // No arguments, list branches for tenant
        let branches = pageserver.branch_list(&tenantid)?;
        print_branches_tree(branches)?;
    }

    Ok(())
}

fn handle_pg(pg_match: &ArgMatches, env: &local_env::LocalEnv) -> Result<()> {
    let (sub_name, sub_args) = match pg_match.subcommand() {
        Some(pg_subcommand_data) => pg_subcommand_data,
        None => bail!("no pg subcommand provided"),
    };

    let mut cplane = ComputeControlPlane::load(env.clone())?;

    // All subcommands take an optional --tenantid option
    let tenantid = get_tenantid(sub_args, env)?;

    match sub_name {
        "list" => {
            let branch_infos = get_branch_infos(env, &tenantid).unwrap_or_else(|e| {
                eprintln!("Failed to load branch info: {}", e);
                HashMap::new()
            });

            println!("NODE\tADDRESS\t\tBRANCH\tLSN\t\tSTATUS");
            for ((_, node_name), node) in cplane
                .nodes
                .iter()
                .filter(|((node_tenantid, _), _)| node_tenantid == &tenantid)
            {
                // FIXME: This shows the LSN at the end of the timeline. It's not the
                // right thing to do for read-only nodes that might be anchored at an
                // older point in time, or following but lagging behind the primary.
                let lsn_str = branch_infos
                    .get(&node.timelineid)
                    .map(|bi| bi.latest_valid_lsn.to_string())
                    .unwrap_or_else(|| "?".to_string());

                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    node_name,
                    node.address,
                    node.timelineid, // FIXME: resolve human-friendly branch name
                    lsn_str,
                    node.status(),
                );
            }
        }
        "create" => {
            let node_name = sub_args.value_of("node").unwrap_or("main");
            let timeline_name = sub_args.value_of("timeline").unwrap_or(node_name);

            let port: Option<u16> = match sub_args.value_of("port") {
                Some(p) => Some(p.parse()?),
                None => None,
            };
            cplane.new_node(tenantid, node_name, timeline_name, port)?;
        }
        "start" => {
            let node_name = sub_args.value_of("node").unwrap_or("main");
            let timeline_name = sub_args.value_of("timeline");

            let port: Option<u16> = match sub_args.value_of("port") {
                Some(p) => Some(p.parse()?),
                None => None,
            };

            let node = cplane.nodes.get(&(tenantid, node_name.to_owned()));

            let auth_token = if matches!(env.pageserver.auth_type, AuthType::ZenithJWT) {
                let claims = Claims::new(Some(tenantid), Scope::Tenant);

                Some(env.generate_auth_token(&claims)?)
            } else {
                None
            };

            if let Some(node) = node {
                if timeline_name.is_some() {
                    println!("timeline name ignored because node exists already");
                }
                println!("Starting existing postgres {}...", node_name);
                node.start(&auth_token)?;
            } else {
                // when used with custom port this results in non obvious behaviour
                // port is remembered from first start command, i e
                // start --port X
                // stop
                // start <-- will also use port X even without explicit port argument
                let timeline_name = timeline_name.unwrap_or(node_name);
                println!(
                    "Starting new postgres {} on {}...",
                    node_name, timeline_name
                );
                let node = cplane.new_node(tenantid, node_name, timeline_name, port)?;
                node.start(&auth_token)?;
            }
        }
        "start-mr" => {
            let node_name = sub_args.value_of("node").unwrap_or("main");
            let global_timeline_name = sub_args.value_of("global").unwrap();
            let region_specs = sub_args.value_of("regions");

            let port: Option<u16> = match sub_args.value_of("port") {
                Some(p) => Some(p.parse()?),
                None => None,
            };

            let node = cplane.nodes.get(&(tenantid, node_name.to_owned()));

            let auth_token = if matches!(env.pageserver.auth_type, AuthType::ZenithJWT) {
                let claims = Claims::new(Some(tenantid), Scope::Tenant);

                Some(env.generate_auth_token(&claims)?)
            } else {
                None
            };

            if let Some(node) = node {
                if region_specs.is_some() {
                    println!("region specs ignored because node exists already");
                }
                println!("Starting existing postgres {}...", node_name);
                node.start(&auth_token)?;
            } else {
                // when used with custom port this results in non obvious behaviour
                // port is remembered from first start command, i e
                // start --port X
                // stop
                // start <-- will also use port X even without explicit port argument
                let region_specs = region_specs.unwrap();
                println!(
                    "Starting new postgres {} with region specs {}...",
                    node_name, region_specs
                );
                let node = cplane.new_multi_region_node(
                    tenantid,
                    node_name,
                    global_timeline_name,
                    region_specs,
                    port,
                )?;
                node.start(&auth_token)?;
            }
        }
        "stop" => {
            let node_name = sub_args.value_of("node").unwrap_or("main");
            let destroy = sub_args.is_present("destroy");

            let node = cplane
                .nodes
                .get(&(tenantid, node_name.to_owned()))
                .with_context(|| format!("postgres {} is not found", node_name))?;
            node.stop(destroy)?;
        }

        _ => {
            bail!("Unexpected pg subcommand '{}'", sub_name)
        }
    }

    Ok(())
}

fn handle_pageserver(sub_match: &ArgMatches, env: &local_env::LocalEnv) -> Result<()> {
    let pageserver = PageServerNode::from_env(env);

    match sub_match.subcommand() {
        Some(("start", start_match)) => {
            if let Err(e) = pageserver.start(&pageserver_config_overrides(start_match)) {
                eprintln!("pageserver start failed: {}", e);
                exit(1);
            }
        }

        Some(("stop", stop_match)) => {
            let immediate = stop_match.value_of("stop-mode") == Some("immediate");

            if let Err(e) = pageserver.stop(immediate) {
                eprintln!("pageserver stop failed: {}", e);
                exit(1);
            }
        }

        Some(("restart", restart_match)) => {
            //TODO what shutdown strategy should we use here?
            if let Err(e) = pageserver.stop(false) {
                eprintln!("pageserver stop failed: {}", e);
                exit(1);
            }

            if let Err(e) = pageserver.start(&pageserver_config_overrides(restart_match)) {
                eprintln!("pageserver start failed: {}", e);
                exit(1);
            }
        }
        Some((sub_name, _)) => bail!("Unexpected pageserver subcommand '{}'", sub_name),
        None => bail!("no pageserver subcommand provided"),
    }
    Ok(())
}

fn get_safekeeper(env: &local_env::LocalEnv, name: &str) -> Result<SafekeeperNode> {
    if let Some(node) = env.safekeepers.iter().find(|node| node.name == name) {
        Ok(SafekeeperNode::from_env(env, node))
    } else {
        bail!("could not find safekeeper '{}'", name)
    }
}

fn handle_safekeeper(sub_match: &ArgMatches, env: &local_env::LocalEnv) -> Result<()> {
    let (sub_name, sub_args) = match sub_match.subcommand() {
        Some(safekeeper_command_data) => safekeeper_command_data,
        None => bail!("no safekeeper subcommand provided"),
    };

    // All the commands take an optional safekeeper name argument
    let node_name = sub_args.value_of("node").unwrap_or(DEFAULT_SAFEKEEPER_NAME);
    let safekeeper = get_safekeeper(env, node_name)?;

    match sub_name {
        "start" => {
            if let Err(e) = safekeeper.start() {
                eprintln!("safekeeper start failed: {}", e);
                exit(1);
            }
        }

        "stop" => {
            let immediate = sub_args.value_of("stop-mode") == Some("immediate");

            if let Err(e) = safekeeper.stop(immediate) {
                eprintln!("safekeeper stop failed: {}", e);
                exit(1);
            }
        }

        "restart" => {
            let immediate = sub_args.value_of("stop-mode") == Some("immediate");

            if let Err(e) = safekeeper.stop(immediate) {
                eprintln!("safekeeper stop failed: {}", e);
                exit(1);
            }

            if let Err(e) = safekeeper.start() {
                eprintln!("safekeeper start failed: {}", e);
                exit(1);
            }
        }

        _ => {
            bail!("Unexpected safekeeper subcommand '{}'", sub_name)
        }
    }
    Ok(())
}

fn handle_start_all(sub_match: &ArgMatches, env: &local_env::LocalEnv) -> Result<()> {
    let pageserver = PageServerNode::from_env(env);

    // Postgres nodes are not started automatically

    if let Err(e) = pageserver.start(&pageserver_config_overrides(sub_match)) {
        eprintln!("pageserver start failed: {}", e);
        exit(1);
    }

    for node in env.safekeepers.iter() {
        let safekeeper = SafekeeperNode::from_env(env, node);
        if let Err(e) = safekeeper.start() {
            eprintln!("safekeeper '{}' start failed: {}", safekeeper.name, e);
            exit(1);
        }
    }
    Ok(())
}

fn handle_stop_all(sub_match: &ArgMatches, env: &local_env::LocalEnv) -> Result<()> {
    let immediate = sub_match.value_of("stop-mode") == Some("immediate");

    let pageserver = PageServerNode::from_env(env);

    // Stop all compute nodes
    let cplane = ComputeControlPlane::load(env.clone())?;
    for (_k, node) in cplane.nodes {
        if let Err(e) = node.stop(false) {
            eprintln!("postgres stop failed: {}", e);
        }
    }

    if let Err(e) = pageserver.stop(immediate) {
        eprintln!("pageserver stop failed: {}", e);
    }

    for node in env.safekeepers.iter() {
        let safekeeper = SafekeeperNode::from_env(env, node);
        if let Err(e) = safekeeper.stop(immediate) {
            eprintln!("safekeeper '{}' stop failed: {}", safekeeper.name, e);
        }
    }
    Ok(())
}
