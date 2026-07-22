use crate::i18n::Lang;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Currency {
    Eur,
    Usd,
    Try,
    Cny,
    Jpy,
    Thb,
    Rub,
    Gbp,
}

impl Currency {
    pub const ALL: [Currency; 8] = [
        Currency::Eur,
        Currency::Usd,
        Currency::Gbp,
        Currency::Try,
        Currency::Cny,
        Currency::Jpy,
        Currency::Thb,
        Currency::Rub,
    ];

    pub fn default_for_lang(lang: Lang) -> Self {
        match lang {
            Lang::El | Lang::Es | Lang::Fr => Currency::Eur,
            Lang::En => Currency::Usd,
            Lang::Tr => Currency::Try,
            Lang::Zh => Currency::Cny,
            Lang::Ja => Currency::Jpy,
            Lang::Th => Currency::Thb,
            Lang::Ru => Currency::Rub,
        }
    }

    pub fn from_code(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "eur" | "€" => Some(Currency::Eur),
            "usd" | "$" => Some(Currency::Usd),
            "gbp" | "£" => Some(Currency::Gbp),
            "try" | "₺" => Some(Currency::Try),
            "cny" | "¥" | "rmb" => Some(Currency::Cny),
            "jpy" => Some(Currency::Jpy),
            "thb" | "฿" => Some(Currency::Thb),
            "rub" | "₽" => Some(Currency::Rub),
            _ => None,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Currency::Eur => "eur",
            Currency::Usd => "usd",
            Currency::Gbp => "gbp",
            Currency::Try => "try",
            Currency::Cny => "cny",
            Currency::Jpy => "jpy",
            Currency::Thb => "thb",
            Currency::Rub => "rub",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Currency::Eur => "EUR €",
            Currency::Usd => "USD $",
            Currency::Gbp => "GBP £",
            Currency::Try => "TRY ₺",
            Currency::Cny => "CNY ¥",
            Currency::Jpy => "JPY ¥",
            Currency::Thb => "THB ฿",
            Currency::Rub => "RUB ₽",
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            Currency::Eur => "€",
            Currency::Usd => "$",
            Currency::Gbp => "£",
            Currency::Try => "₺",
            Currency::Cny => "¥",
            Currency::Jpy => "¥",
            Currency::Thb => "฿",
            Currency::Rub => "₽",
        }
    }

    /// Units of this currency per 1 EUR (approximate, for UI conversion).
    fn per_eur(self) -> f64 {
        match self {
            Currency::Eur => 1.0,
            Currency::Usd => 1.08,
            Currency::Gbp => 0.86,
            Currency::Try => 35.0,
            Currency::Cny => 7.8,
            Currency::Jpy => 162.0,
            Currency::Thb => 38.0,
            Currency::Rub => 98.0,
        }
    }

    pub fn convert(amount: f64, from: Self, to: Self) -> f64 {
        if from == to {
            return amount;
        }
        let eur = amount / from.per_eur();
        eur * to.per_eur()
    }

    pub fn power_cost_range(self) -> (f32, f32) {
        match self {
            Currency::Eur => (0.05, 0.45),
            Currency::Usd => (0.05, 0.50),
            Currency::Gbp => (0.04, 0.40),
            Currency::Try => (1.0, 18.0),
            Currency::Cny => (0.3, 2.5),
            Currency::Jpy => (10.0, 80.0),
            Currency::Thb => (2.0, 12.0),
            Currency::Rub => (1.0, 12.0),
        }
    }

    pub fn default_power_cost(self) -> f32 {
        match self {
            Currency::Eur => 0.15,
            Currency::Usd => 0.16,
            Currency::Gbp => 0.13,
            Currency::Try => 4.0,
            Currency::Cny => 0.75,
            Currency::Jpy => 28.0,
            Currency::Thb => 5.0,
            Currency::Rub => 2.8,
        }
    }

    pub fn slider_step(self) -> f32 {
        match self {
            Currency::Eur | Currency::Usd | Currency::Gbp => 0.01,
            Currency::Try | Currency::Thb | Currency::Rub => 0.1,
            Currency::Cny => 0.05,
            Currency::Jpy => 1.0,
        }
    }

    pub fn format_amount(self, amount: f64) -> String {
        let decimals = match self {
            Currency::Eur | Currency::Usd | Currency::Gbp | Currency::Cny => 2,
            Currency::Try | Currency::Thb | Currency::Rub => 2,
            Currency::Jpy => 0,
        };
        format!("{amount:.decimals$} {}", self.symbol())
    }
}

pub fn load_currency(work_dir: &std::path::Path, lang: Lang) -> Currency {
    let path = work_dir.join("miner-panel.currency");
    if let Ok(s) = std::fs::read_to_string(path) {
        if let Some(c) = Currency::from_code(&s) {
            return c;
        }
    }
    Currency::default_for_lang(lang)
}

pub fn save_currency(work_dir: &std::path::Path, currency: Currency) {
    let path = work_dir.join("miner-panel.currency");
    let _ = std::fs::write(path, currency.code());
}
