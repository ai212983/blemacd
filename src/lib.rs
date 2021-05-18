use std::slice::Iter;

pub mod handlers;
pub mod shutting_down_stream;

#[derive(Hash, Eq, PartialEq, Debug, Clone, Copy)]
pub enum Position {
    Init,
    Root,
    Device,
    Service,
    Characteristic,
}

impl Position {
    const COUNT: usize = 5;

    pub fn iter() -> Iter<'static, Position> {
        static POSITIONS: [Position; Position::COUNT] = [
            Position::Init,
            Position::Root,
            Position::Device,
            Position::Service,
            Position::Characteristic,
        ];
        POSITIONS.iter()
    }

    pub fn count() -> usize {
        return Position::COUNT;
    }
}
