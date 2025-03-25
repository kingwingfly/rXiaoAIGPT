use std::fmt;

#[derive(Debug, Default, Clone, Copy)]
pub enum Sid {
    #[default]
    Micoapi,
    Xiaomiio,
}

impl fmt::Display for Sid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Sid::Micoapi => write!(f, "micoapi"),
            Sid::Xiaomiio => write!(f, "xiaomiio"),
        }
    }
}
