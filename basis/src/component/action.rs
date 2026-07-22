pub const TX_ACTIONS_MAX: usize = 200;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ExecFrom {
    Top,
    Ast,
    Call,
}

impl ExecFrom {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Top => "TOP",
            Self::Ast => "AST",
            Self::Call => "CALL",
        }
    }
}

impl core::fmt::Display for ExecFrom {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum TopRule {
    None,
    Only,
    OnlyCanWithGuard,
    Unique,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum ScopeKind {
    Top,
    Ast,
    Guard,
    Call,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum ExecPolicy {
    TopOnly,
    TopAndAst,
    Anywhere,
    CallOnly,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct ActScope {
    kind: ScopeKind,
    exec: ExecPolicy,
    top: TopRule,
}

impl ActScope {
    pub const TOP: Self = Self {
        kind: ScopeKind::Top,
        exec: ExecPolicy::TopOnly,
        top: TopRule::None,
    };
    pub const TOP_ONLY: Self = Self {
        kind: ScopeKind::Top,
        exec: ExecPolicy::TopOnly,
        top: TopRule::Only,
    };
    pub const TOP_ONLY_CAN_WITH_GUARD: Self = Self {
        kind: ScopeKind::Top,
        exec: ExecPolicy::TopOnly,
        top: TopRule::OnlyCanWithGuard,
    };
    pub const TOP_UNIQUE: Self = Self {
        kind: ScopeKind::Top,
        exec: ExecPolicy::TopOnly,
        top: TopRule::Unique,
    };
    pub const AST: Self = Self {
        kind: ScopeKind::Ast,
        exec: ExecPolicy::TopAndAst,
        top: TopRule::None,
    };
    pub const GUARD: Self = Self {
        kind: ScopeKind::Guard,
        exec: ExecPolicy::TopAndAst,
        top: TopRule::None,
    };
    /// Top-level-only guard companion (may appear multiple times unless Unique).
    pub const TOP_GUARD: Self = Self {
        kind: ScopeKind::Guard,
        exec: ExecPolicy::TopOnly,
        top: TopRule::None,
    };
    /// Top-level-only guard companion; at most one action of this kind.
    pub const TOP_GUARD_UNIQUE: Self = Self {
        kind: ScopeKind::Guard,
        exec: ExecPolicy::TopOnly,
        top: TopRule::Unique,
    };
    pub const CALL: Self = Self {
        kind: ScopeKind::Call,
        exec: ExecPolicy::Anywhere,
        top: TopRule::None,
    };
    pub const CALL_ONLY: Self = Self {
        kind: ScopeKind::Call,
        exec: ExecPolicy::CallOnly,
        top: TopRule::None,
    };

    pub const fn is_guard(self) -> bool {
        matches!(self.kind, ScopeKind::Guard)
    }

    pub const fn top_rule(self) -> Option<TopRule> {
        match self.kind {
            ScopeKind::Top => Some(self.top),
            // TOP_GUARD* may carry TopRule::Unique (or future Only*).
            ScopeKind::Guard if !matches!(self.top, TopRule::None) => Some(self.top),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match (self.kind, self.top, self.exec) {
            (ScopeKind::Top, TopRule::None, _) => "TOP",
            (ScopeKind::Top, TopRule::Only, _) => "TOP_ONLY",
            (ScopeKind::Top, TopRule::OnlyCanWithGuard, _) => "TOP_ONLY_CAN_WITH_GUARD",
            (ScopeKind::Top, TopRule::Unique, _) => "TOP_UNIQUE",
            (ScopeKind::Ast, _, _) => "AST",
            (ScopeKind::Guard, TopRule::None, ExecPolicy::TopOnly) => "TOP_GUARD",
            (ScopeKind::Guard, TopRule::Unique, ExecPolicy::TopOnly) => "TOP_GUARD_UNIQUE",
            (ScopeKind::Guard, _, _) => "GUARD",
            (ScopeKind::Call, _, ExecPolicy::Anywhere) => "CALL",
            (ScopeKind::Call, _, ExecPolicy::CallOnly) => "CALL_ONLY",
            _ => "UNKNOWN",
        }
    }

    pub const fn allows(self, from: ExecFrom) -> bool {
        match self.exec {
            ExecPolicy::TopOnly => matches!(from, ExecFrom::Top),
            ExecPolicy::TopAndAst => !matches!(from, ExecFrom::Call),
            ExecPolicy::Anywhere => true,
            ExecPolicy::CallOnly => matches!(from, ExecFrom::Call),
        }
    }
}

impl core::fmt::Display for ActScope {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}
