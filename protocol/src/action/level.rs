// Action tree shape used by level checking. "Terminal" means the node has no AST children.
enum ActionShape<'a> {
    Terminal,
    AstContainer {
        depth_inc: usize,
        children: Vec<&'a dyn Action>,
    },
}

#[derive(Default)]
// Aggregated tx-level topology facts collected from the whole action forest.
struct TxTopologyStats {
    top_action_count: usize,
    top_kind_count: std::collections::HashMap<u16, usize>,
    top_guard_count: usize,
    non_guard_terminal_count: usize,
    has_guard_terminal: bool,
    max_ast_depth: usize,
}

impl TxTopologyStats {
    fn record_top_action(&mut self, act: &dyn Action) {
        self.top_action_count += 1;
        *self.top_kind_count.entry(act.kind()).or_insert(0) += 1;
        if act.scope().is_guard() {
            self.top_guard_count += 1;
        }
    }

    fn record_terminal_action(&mut self, scope: ActScope) {
        if scope.is_guard() {
            self.has_guard_terminal = true;
        } else {
            self.non_guard_terminal_count += 1;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxActionTopology {
    pub top_action_count: usize,
    pub top_kind_count: Vec<(u16, usize)>,
    pub top_guard_count: usize,
    pub non_guard_terminal_count: usize,
    pub has_guard_terminal: bool,
    pub max_ast_depth: usize,
}

impl From<&TxTopologyStats> for TxActionTopology {
    fn from(stats: &TxTopologyStats) -> Self {
        let mut top_kind_count: Vec<_> = stats
            .top_kind_count
            .iter()
            .map(|(kind, count)| (*kind, *count))
            .collect();
        top_kind_count.sort_by_key(|(kind, _)| *kind);
        Self {
            top_action_count: stats.top_action_count,
            top_kind_count,
            top_guard_count: stats.top_guard_count,
            non_guard_terminal_count: stats.non_guard_terminal_count,
            has_guard_terminal: stats.has_guard_terminal,
            max_ast_depth: stats.max_ast_depth,
        }
    }
}

fn action_shape<'a>(act: &'a dyn Action) -> ActionShape<'a> {
    match get_action_level_inc_and_childs(act) {
        Some((depth_inc, children)) => ActionShape::AstContainer {
            depth_inc,
            children,
        },
        None => ActionShape::Terminal,
    }
}

fn validate_tx_action_count(actions: &[Box<dyn Action>]) -> Rerr {
    let actlen = actions.len();
    if actlen < 1 || actlen > TX_ACTIONS_MAX {
        return errf!(
            "action length {} is 0 or one transaction max actions is {}",
            actlen,
            TX_ACTIONS_MAX
        );
    }
    Ok(())
}

fn validate_depth_limit(ast_depth: usize, depth_inc: usize) -> Ret<usize> {
    let next_depth = ast_depth
        .checked_add(depth_inc)
        .ok_or_else(|| "ast tree depth overflow".to_string())?;
    if next_depth > AST_TREE_DEPTH_MAX {
        return errf!(
            "ast tree depth {} exceeded max {}",
            next_depth,
            AST_TREE_DEPTH_MAX
        );
    }
    Ok(next_depth)
}

fn validate_node_tx_type(tx_type: u8, act: &dyn Action) -> Rerr {
    let min_tx_type = act.min_tx_type();
    if tx_type < min_tx_type {
        return errf!(
            "action node invalid: action {} requires tx type >= {} but current tx type is {}",
            act.kind(),
            min_tx_type,
            tx_type
        );
    }
    Ok(())
}

fn validate_node_scope(origin: ExecFrom, act: &dyn Action) -> Rerr {
    let scope = act.scope();
    if !scope.allows(origin) {
        return errf!(
            "action node invalid: action {} with scope {} not allowed from {}",
            act.kind(),
            scope,
            origin
        );
    }
    Ok(())
}

fn validate_current_node(tx_type: u8, act: &dyn Action, origin: ExecFrom) -> Rerr {
    validate_node_scope(origin, act)?;
    validate_node_tx_type(tx_type, act)?;
    Ok(())
}

fn collect_tx_topology(
    act: &dyn Action,
    origin: ExecFrom,
    shape: &ActionShape,
    stats: &mut TxTopologyStats,
) {
    if origin == ExecFrom::Top {
        stats.record_top_action(act);
    }
    if matches!(shape, ActionShape::Terminal) {
        stats.record_terminal_action(act.scope());
    }
}

fn visit_tx_node(
    tx_type: u8,
    act: &dyn Action,
    origin: ExecFrom,
    ast_depth: usize,
    stats: &mut TxTopologyStats,
) -> Rerr {
    validate_current_node(tx_type, act, origin)?;
    stats.max_ast_depth = stats.max_ast_depth.max(ast_depth);
    let shape = action_shape(act);
    collect_tx_topology(act, origin, &shape, stats);
    match shape {
        ActionShape::Terminal => Ok(()),
        ActionShape::AstContainer {
            depth_inc,
            children,
        } => {
            let next_depth = validate_depth_limit(ast_depth, depth_inc)?;
            for sub in children {
                visit_tx_node(tx_type, sub, ExecFrom::Ast, next_depth, stats)?;
            }
            Ok(())
        }
    }
}

fn visit_runtime_node(
    tx_type: u8,
    act: &dyn Action,
    origin: ExecFrom,
    ast_depth: usize,
) -> Rerr {
    validate_current_node(tx_type, act, origin)?;
    match action_shape(act) {
        ActionShape::Terminal => Ok(()),
        ActionShape::AstContainer {
            depth_inc,
            children,
        } => {
            let next_depth = validate_depth_limit(ast_depth, depth_inc)?;
            for sub in children {
                visit_runtime_node(tx_type, sub, ExecFrom::Ast, next_depth)?;
            }
            Ok(())
        }
    }
}

fn visit_runtime_node_fast_sync(tx_type: u8, act: &dyn Action, ast_depth: usize) -> Rerr {
    validate_node_tx_type(tx_type, act)?;
    match action_shape(act) {
        ActionShape::Terminal => Ok(()),
        ActionShape::AstContainer {
            depth_inc,
            children,
        } => {
            let next_depth = validate_depth_limit(ast_depth, depth_inc)?;
            for sub in children {
                visit_runtime_node_fast_sync(tx_type, sub, next_depth)?;
            }
            Ok(())
        }
    }
}

fn validate_no_guard_only_tx(stats: &TxTopologyStats) -> Rerr {
    if stats.has_guard_terminal && stats.non_guard_terminal_count == 0 {
        return errf!("tx topology invalid: tx actions cannot be all GUARD");
    }
    Ok(())
}

fn validate_top_only_rule(act: &dyn Action, stats: &TxTopologyStats) -> Rerr {
    if stats.top_action_count != 1 {
        return errf!("tx topology invalid: action {} can only execute on TOP_ONLY", act.kind());
    }
    Ok(())
}

fn validate_top_only_can_with_guard_rule(act: &dyn Action, stats: &TxTopologyStats) -> Rerr {
    if stats.top_action_count != 1 + stats.top_guard_count
        || stats.non_guard_terminal_count != 1
    {
        return errf!(
            "tx topology invalid: action {} can only execute on TOP_ONLY_CAN_WITH_GUARD (requires exactly one non-guard leaf action plus optional bare top-level GUARD actions)",
            act.kind()
        );
    }
    Ok(())
}

fn validate_top_unique_rule(act: &dyn Action, stats: &TxTopologyStats) -> Rerr {
    if stats.top_kind_count.get(&act.kind()).copied().unwrap_or(0) != 1 {
        return errf!("tx topology invalid: action {} can only execute on TOP_UNIQUE", act.kind());
    }
    Ok(())
}

fn validate_top_rule_for_action(act: &dyn Action, stats: &TxTopologyStats) -> Rerr {
    let Some(rule) = act.scope().top_rule() else {
        return Ok(());
    };
    match rule {
        TopRule::None => Ok(()),
        TopRule::Only => validate_top_only_rule(act, stats),
        TopRule::OnlyCanWithGuard => validate_top_only_can_with_guard_rule(act, stats),
        TopRule::Unique => validate_top_unique_rule(act, stats),
    }
}

fn validate_tx_topology(actions: &[Box<dyn Action>], stats: &TxTopologyStats) -> Rerr {
    validate_no_guard_only_tx(stats)?;
    for act in actions {
        validate_top_rule_for_action(act.as_ref(), stats)?;
    }
    Ok(())
}

pub fn precheck_tx_actions(tx_type: u8, actions: &[Box<dyn Action>]) -> Rerr {
    validate_tx_action_count(actions)?;
    let mut stats = TxTopologyStats::default();
    for act in actions {
        visit_tx_node(tx_type, act.as_ref(), ExecFrom::Top, 0, &mut stats)?;
    }
    validate_tx_topology(actions, &stats)
}

pub fn precheck_tx_actions_report(
    tx_type: u8,
    actions: &[Box<dyn Action>],
) -> Ret<TxActionTopology> {
    validate_tx_action_count(actions)?;
    let mut stats = TxTopologyStats::default();
    for act in actions {
        visit_tx_node(tx_type, act.as_ref(), ExecFrom::Top, 0, &mut stats)?;
    }
    validate_tx_topology(actions, &stats)?;
    Ok(TxActionTopology::from(&stats))
}

pub fn precheck_runtime_action(tx_type: u8, act: &dyn Action, from: ExecFrom) -> Rerr {
    visit_runtime_node(tx_type, act, from, 0)
}

pub fn precheck_runtime_action_fast_sync(tx_type: u8, act: &dyn Action) -> Rerr {
    visit_runtime_node_fast_sync(tx_type, act, 0)
}
