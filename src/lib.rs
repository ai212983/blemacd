use std::slice::Iter;

pub mod handlers;
pub mod central_async;

#[derive(Hash, Eq, PartialEq, Debug, Clone, Copy)]
pub enum Position {
    Root,
    Devices,
    Services,
    Characteristics,
}

impl Position {
    pub fn iter() -> Iter<'static, Position> {
        static POSITIONS: [Position; 4] = [
            Position::Root,
            Position::Devices,
            Position::Services,
            Position::Characteristics,
        ];
        POSITIONS.iter()
    }
    pub fn count() -> u32 {
        return 4;
    }
}
