#[derive(Debug, Clone)]
pub struct FlagDef {
    pub long: String,
    pub short: Option<char>,
    pub takes_value: bool,
}

#[derive(Debug, Default, Clone)]
pub struct FlagSpec {
    pub defs: Vec<FlagDef>,
}

impl FlagSpec {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn flag(mut self, long: &str, short: Option<char>, takes_value: bool) -> Self {
        self.defs.push(FlagDef {
            long: long.to_string(),
            short,
            takes_value,
        });
        self
    }

    pub fn find_long(&self, name: &str) -> Option<&FlagDef> {
        self.defs.iter().find(|d| d.long == name)
    }

    pub fn find_short(&self, ch: char) -> Option<&FlagDef> {
        self.defs.iter().find(|d| d.short == Some(ch))
    }
}
