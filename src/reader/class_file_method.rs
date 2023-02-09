use std::fmt;
use std::fmt::Formatter;

use crate::reader::method_descriptor::MethodDescriptor;
use crate::reader::{attribute::Attribute, instruction::Instruction, method_flags::MethodFlags};

#[derive(Debug, Default, PartialEq)]
pub struct ClassFileMethod {
    pub flags: MethodFlags,
    pub name: String,
    pub type_descriptor: String,
    pub parsed_type_descriptor: MethodDescriptor,
    pub attributes: Vec<Attribute>,
    pub code: ClassFileMethodCode,
}

impl fmt::Display for ClassFileMethod {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{:?} {}: {}",
            self.flags, self.name, self.parsed_type_descriptor,
        )?;
        writeln!(f, "  code: {}", self.code)?;
        write!(f, "  raw_attributes: {:?}", self.attributes)
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct ClassFileMethodCode {
    pub max_stack: u16,
    pub max_locals: u16,
    pub code: Vec<Instruction>,
    pub exception_table: Vec<u8>, // TODO: replace with some proper struct
    pub attributes: Vec<Attribute>, // TODO: replace with some proper struct
}

impl fmt::Display for ClassFileMethodCode {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "max_stack = {}, max_locals = {}, exception_table = {:?}, attributes = {:?}, instructions:",
            self.max_stack, self.max_locals, self.exception_table, self.attributes
        )?;
        for instruction in self.code.iter() {
            writeln!(f, "    {instruction}")?;
        }
        Ok(())
    }
}