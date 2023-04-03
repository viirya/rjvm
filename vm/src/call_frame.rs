use std::{cell::RefCell, rc::Rc};

use log::{debug, warn};

use rjvm_reader::{
    class_file_field::ClassFileField,
    class_file_method::ClassFileMethod,
    constant_pool::ConstantPoolEntry,
    field_type::{BaseType, FieldType, FieldType::Base},
    instruction::{Instruction, NewArrayType},
};
use rjvm_utils::type_conversion::ToUsizeSafe;

use crate::{
    call_stack::CallStack,
    class::Class,
    class_and_method::ClassAndMethod,
    value::{
        ArrayRef, ObjectRef, Value,
        Value::{Array, Double, Float, Int, Long, Object},
    },
    value_stack::ValueStack,
    vm::Vm,
    vm_error::VmError,
};

macro_rules! generate_pop {
    ($name:ident, $variant:ident, $type:ty) => {
        fn $name(&mut self) -> Result<$type, VmError> {
            let value = self.stack.pop()?;
            match value {
                $variant(value) => Ok(value),
                _ => Err(VmError::ValidationException),
            }
        }
    };
}

macro_rules! generate_execute_return {
    ($name:ident, $variant:ident) => {
        fn $name(&mut self) -> Result<Option<Value<'a>>, VmError> {
            if !self.class_and_method.returns(Base(BaseType::$variant)) {
                return Err(VmError::ValidationException);
            }
            let result = self.stack.pop()?;
            self.debug_done_execution(Some(&result));
            return Ok(Some(result));
        }
    };
}

macro_rules! generate_execute_math {
    ($name:ident, $pop_fn:ident, $variant:ident, $type:ty) => {
        fn $name<T>(&mut self, evaluator: T) -> Result<(), VmError>
        where
            T: FnOnce($type, $type) -> Result<$type, VmError>,
        {
            let val2 = self.$pop_fn()?;
            let val1 = self.$pop_fn()?;
            let result = evaluator(val1, val2)?;
            self.stack.push($variant(result))
        }
    };
}

macro_rules! generate_execute_neg {
    ($name:ident, $pop_fn:ident, $variant:ident) => {
        fn $name(&mut self) -> Result<(), VmError> {
            let value = self.$pop_fn()?;
            self.stack.push($variant(-value))
        }
    };
}

macro_rules! generate_execute_coerce {
    ($name:ident, $pop_fn:ident, $type:ty) => {
        fn $name<T>(&mut self, evaluator: T) -> Result<(), VmError>
        where
            T: FnOnce($type) -> Value<'a>,
        {
            let value = self.$pop_fn()?;
            let coerced = evaluator(value);
            self.stack.push(coerced)
        }
    };
}

macro_rules! generate_compare {
    ($name:ident, $pop_fn:ident) => {
        fn $name(&mut self, sign_for_greater: i32) -> Result<(), VmError> {
            let val2 = self.$pop_fn()?;
            let val1 = self.$pop_fn()?;
            if val1 > val2 {
                self.stack.push(Int(sign_for_greater))
            } else if val1 < val2 {
                self.stack.push(Int(-sign_for_greater))
            } else {
                self.stack.push(Int(0))
            }
        }
    };
}

macro_rules! generate_execute_load {
    ($name:ident, $($variant:ident),+) => {
        fn $name(&mut self, index: usize) -> Result<(), VmError> {
            let local = self.locals.get(index).ok_or(VmError::ValidationException)?;
            match local {
                $($variant(..) => {
                    self.stack.push(local.clone())
                }),+
                _ => Err(VmError::ValidationException),
            }
        }
    };
}

macro_rules! generate_execute_store {
    ($name:ident, $($variant:ident),+) => {
        fn $name(&mut self, index: usize) -> Result<(), VmError> {
            let value = self.stack.pop()?;
            match value {
                $($variant(..) => {
                    self.locals[index] = value;
                    Ok(())
                })+
                _ => Err(VmError::ValidationException),
            }
        }
    };
}

macro_rules! generate_execute_array_load {
    ($name:ident, $($variant:pat),+) => {
        fn $name(&mut self) -> Result<(), VmError> {
            let index = self.pop_int()?.into_usize_safe();
            let (field_type, array) = self.pop_array()?;
            let value = match field_type {
                $($variant => array
                    .borrow()
                    .get(index)
                    .ok_or(VmError::ArrayIndexOutOfBoundsException)
                    .map(|value| value.clone()),)+
                _ => return Err(VmError::ValidationException),
            }?;
            self.stack.push(value)
        }
    };
}

macro_rules! generate_execute_array_store {
    ($name:ident, $pop_fn:ident, $map_fn:ident, $($variant:pat),+) => {
        fn $name(&mut self) -> Result<(), VmError> {
            let value = Self::$map_fn(self.$pop_fn()?);
            let index = self.pop_int()?.into_usize_safe();
            let (field_type, array) = self.pop_array()?;
            match field_type {
                $($variant => {
                    match array.borrow_mut().get_mut(index) {
                        None => return Err(VmError::ArrayIndexOutOfBoundsException),
                        Some(reference) => *reference = value,
                    }
                })+
                _ => return Err(VmError::ValidationException),
            }
            Ok(())
        }
    };
}

#[derive(Debug)]
struct MethodReference<'a> {
    class_name: &'a str,
    method_name: &'a str,
    type_descriptor: &'a str,
}

#[derive(Debug)]
struct FieldReference<'a> {
    class_name: &'a str,
    field_name: &'a str,
    #[allow(dead_code)]
    type_descriptor: &'a str,
}

#[derive(Debug)]
pub struct CallFrame<'a> {
    class_and_method: ClassAndMethod<'a>,
    pc: usize,
    locals: Vec<Value<'a>>,
    stack: ValueStack<'a>,
    code: &'a Vec<u8>,
}

#[derive(Clone, Copy)]
enum InvokeKind {
    Special,
    Static,
    Virtual,
}

impl<'a> CallFrame<'a> {
    pub fn new(class_and_method: ClassAndMethod<'a>, locals: Vec<Value<'a>>) -> Self {
        let max_stack_size = class_and_method
            .method
            .code
            .as_ref()
            .expect("method is not native")
            .max_stack
            .into_usize_safe();
        let code = &class_and_method
            .method
            .code
            .as_ref()
            .expect("method is not native")
            .code;
        CallFrame {
            class_and_method,
            pc: 0,
            locals,
            stack: ValueStack::with_max_size(max_stack_size),
            code,
        }
    }

    pub fn execute(
        &mut self,
        vm: &mut Vm<'a>,
        call_stack: &mut CallStack<'a>,
    ) -> Result<Option<Value<'a>>, VmError> {
        self.debug_start_execution();

        loop {
            let (instruction, new_address) =
                Instruction::parse(self.code, self.pc).map_err(|_| VmError::ValidationException)?;
            self.debug_print_status(&instruction);
            self.pc = new_address;

            match instruction {
                Instruction::Aconst_null => self.stack.push(Value::Null)?,
                Instruction::Aload(index) => self.execute_aload(index.into_usize_safe())?,
                Instruction::Aload_0 => self.execute_aload(0)?,
                Instruction::Aload_1 => self.execute_aload(1)?,
                Instruction::Aload_2 => self.execute_aload(2)?,
                Instruction::Aload_3 => self.execute_aload(3)?,

                Instruction::Astore(index) => self.execute_astore(index.into_usize_safe())?,
                Instruction::Astore_0 => self.execute_astore(0)?,
                Instruction::Astore_1 => self.execute_astore(1)?,
                Instruction::Astore_2 => self.execute_astore(2)?,
                Instruction::Astore_3 => self.execute_astore(3)?,

                Instruction::Iload(index) => self.execute_iload(index.into_usize_safe())?,
                Instruction::Iload_0 => self.execute_iload(0)?,
                Instruction::Iload_1 => self.execute_iload(1)?,
                Instruction::Iload_2 => self.execute_iload(2)?,
                Instruction::Iload_3 => self.execute_iload(3)?,

                Instruction::Istore(index) => self.execute_istore(index.into_usize_safe())?,
                Instruction::Istore_0 => self.execute_istore(0)?,
                Instruction::Istore_1 => self.execute_istore(1)?,
                Instruction::Istore_2 => self.execute_istore(2)?,
                Instruction::Istore_3 => self.execute_istore(3)?,

                Instruction::Iconst_m1 => self.stack.push(Int(-1))?,
                Instruction::Iconst_0 => self.stack.push(Int(0))?,
                Instruction::Iconst_1 => self.stack.push(Int(1))?,
                Instruction::Iconst_2 => self.stack.push(Int(2))?,
                Instruction::Iconst_3 => self.stack.push(Int(3))?,
                Instruction::Iconst_4 => self.stack.push(Int(4))?,
                Instruction::Iconst_5 => self.stack.push(Int(5))?,

                Instruction::Lconst_0 => self.stack.push(Long(0))?,
                Instruction::Lconst_1 => self.stack.push(Long(1))?,

                Instruction::Fconst_0 => self.stack.push(Float(0f32))?,
                Instruction::Fconst_1 => self.stack.push(Float(1f32))?,
                Instruction::Fconst_2 => self.stack.push(Float(2f32))?,

                Instruction::Dconst_0 => self.stack.push(Double(0f64))?,
                Instruction::Dconst_1 => self.stack.push(Double(1f64))?,

                Instruction::Lload(index) => self.execute_lload(index.into_usize_safe())?,
                Instruction::Lload_0 => self.execute_lload(0)?,
                Instruction::Lload_1 => self.execute_lload(1)?,
                Instruction::Lload_2 => self.execute_lload(2)?,
                Instruction::Lload_3 => self.execute_lload(3)?,

                Instruction::Lstore(index) => self.execute_lstore(index.into_usize_safe())?,
                Instruction::Lstore_0 => self.execute_lstore(0)?,
                Instruction::Lstore_1 => self.execute_lstore(1)?,
                Instruction::Lstore_2 => self.execute_lstore(2)?,
                Instruction::Lstore_3 => self.execute_lstore(3)?,

                Instruction::Ldc(index) => self.execute_ldc(vm, index as u16)?,
                Instruction::Ldc_w(index) => self.execute_ldc(vm, index)?,
                Instruction::Ldc2_w(index) => self.execute_ldc_long_double(index)?,

                Instruction::Fload(index) => self.execute_fload(index.into_usize_safe())?,
                Instruction::Fload_0 => self.execute_fload(0)?,
                Instruction::Fload_1 => self.execute_fload(1)?,
                Instruction::Fload_2 => self.execute_fload(2)?,
                Instruction::Fload_3 => self.execute_fload(3)?,

                Instruction::Fstore(index) => self.execute_fstore(index.into_usize_safe())?,
                Instruction::Fstore_0 => self.execute_fstore(0)?,
                Instruction::Fstore_1 => self.execute_fstore(1)?,
                Instruction::Fstore_2 => self.execute_fstore(2)?,
                Instruction::Fstore_3 => self.execute_fstore(3)?,

                Instruction::Dload(index) => self.execute_dload(index.into_usize_safe())?,
                Instruction::Dload_0 => self.execute_dload(0)?,
                Instruction::Dload_1 => self.execute_dload(1)?,
                Instruction::Dload_2 => self.execute_dload(2)?,
                Instruction::Dload_3 => self.execute_dload(3)?,

                Instruction::Dstore(index) => self.execute_dstore(index.into_usize_safe())?,
                Instruction::Dstore_0 => self.execute_dstore(0)?,
                Instruction::Dstore_1 => self.execute_dstore(1)?,
                Instruction::Dstore_2 => self.execute_dstore(2)?,
                Instruction::Dstore_3 => self.execute_dstore(3)?,

                Instruction::I2b => self.coerce_int(Self::i2b)?,
                Instruction::I2c => self.coerce_int(Self::i2c)?,
                Instruction::I2s => self.coerce_int(Self::i2s)?,
                Instruction::I2f => self.coerce_int(Self::i2f)?,
                Instruction::I2l => self.coerce_int(Self::i2l)?,
                Instruction::I2d => self.coerce_int(Self::i2d)?,

                Instruction::L2i => self.coerce_long(Self::l2i)?,
                Instruction::L2f => self.coerce_long(Self::l2f)?,
                Instruction::L2d => self.coerce_long(Self::l2d)?,

                Instruction::F2i => self.coerce_float(Self::f2i)?,
                Instruction::F2l => self.coerce_float(Self::f2l)?,
                Instruction::F2d => self.coerce_float(Self::f2d)?,

                Instruction::D2i => self.coerce_double(Self::d2i)?,
                Instruction::D2l => self.coerce_double(Self::d2l)?,
                Instruction::D2f => self.coerce_double(Self::d2f)?,

                Instruction::New(constant_index) => {
                    let new_object_class_name =
                        self.get_constant_class_reference(constant_index)?;
                    let new_object = vm.new_object(new_object_class_name)?;
                    self.stack.push(Object(new_object))?;
                }

                Instruction::Dup => self.stack.dup()?,
                Instruction::Dup_x1 => self.stack.dup_x1()?,
                Instruction::Dup_x2 => self.stack.dup_x2()?,
                Instruction::Dup2 => self.stack.dup2()?,
                Instruction::Dup2_x1 => self.stack.dup2_x1()?,
                Instruction::Dup2_x2 => self.stack.dup2_x2()?,
                Instruction::Pop => self.stack.pop().map(|_| ())?,
                Instruction::Pop2 => self.stack.pop2().map(|_| ())?,
                Instruction::Swap => self.stack.swap()?,

                Instruction::Bipush(byte_value) => self.stack.push(Int(byte_value as i32))?,
                Instruction::Sipush(short_value) => self.stack.push(Int(short_value as i32))?,

                Instruction::Invokespecial(constant_index) => {
                    self.invoke_method(vm, call_stack, constant_index, InvokeKind::Special)?
                }
                Instruction::Invokestatic(constant_index) => {
                    self.invoke_method(vm, call_stack, constant_index, InvokeKind::Static)?
                }
                Instruction::Invokevirtual(constant_index) => {
                    self.invoke_method(vm, call_stack, constant_index, InvokeKind::Virtual)?
                }

                Instruction::Return => {
                    if !self.class_and_method.is_void() {
                        return Err(VmError::ValidationException);
                    }
                    self.debug_done_execution(None);
                    return Ok(None);
                }
                Instruction::Ireturn => return self.execute_ireturn(),
                Instruction::Lreturn => return self.execute_lreturn(),
                Instruction::Freturn => return self.execute_freturn(),
                Instruction::Dreturn => return self.execute_dreturn(),

                Instruction::Putfield(field_index) => {
                    let field_reference = self.get_constant_field_reference(field_index)?;

                    let (index, field) =
                        Self::get_field(self.class_and_method.class, field_reference)?;

                    let value = self.stack.pop()?;
                    Self::validate_type(vm, field.type_descriptor.clone(), &value)?;
                    let object = self.stack.pop()?;
                    if let Object(object_ref) = object {
                        object_ref.set_field(index, value);
                    } else {
                        return Err(VmError::ValidationException);
                    }
                }

                Instruction::Getfield(field_index) => {
                    let field_reference = self.get_constant_field_reference(field_index)?;

                    let (index, field) =
                        Self::get_field(self.class_and_method.class, field_reference)?;

                    let object = self.stack.pop()?;
                    if let Object(object_ref) = object {
                        let field_value = object_ref.get_field(index);
                        Self::validate_type(vm, field.type_descriptor.clone(), &field_value)?;
                        self.stack.push(field_value)?;
                    } else {
                        return Err(VmError::ValidationException);
                    }
                }

                Instruction::Iadd => self.execute_int_math(|a, b| Ok(a + b))?,
                Instruction::Isub => self.execute_int_math(|a, b| Ok(a - b))?,
                Instruction::Imul => self.execute_int_math(|a, b| Ok(a * b))?,
                Instruction::Idiv => self.execute_int_math(|a, b| match b {
                    0 => Err(VmError::ArithmeticException),
                    _ => Ok(a / b),
                })?,
                Instruction::Irem => self.execute_int_math(|a, b| match b {
                    0 => Err(VmError::ArithmeticException),
                    _ => Ok(a % b),
                })?,
                Instruction::Iand => self.execute_int_math(|a, b| Ok(a & b))?,
                Instruction::Ior => self.execute_int_math(|a, b| Ok(a | b))?,
                Instruction::Ixor => self.execute_int_math(|a, b| Ok(a ^ b))?,
                Instruction::Ishr => self.execute_int_math(|a, b| Ok(a >> (b & 0x1f)))?,
                Instruction::Ishl => self.execute_int_math(|a, b| Ok(a << (b & 0x1f)))?,
                Instruction::Iushr => self.execute_int_math(|a, b| {
                    Ok({
                        if a > 0 {
                            a >> (b & 0x1f)
                        } else {
                            ((a as u32) >> (b & 0x1f)) as i32
                        }
                    })
                })?,

                Instruction::Iinc(index, constant) => {
                    let index = index.into_usize_safe();
                    let local = self.get_local_int_as_int(vm, index)?;
                    self.locals[index] = Int(local + constant as i32);
                }

                Instruction::Ladd => self.execute_long_math(|a, b| Ok(a + b))?,
                Instruction::Lsub => self.execute_long_math(|a, b| Ok(a - b))?,
                Instruction::Lmul => self.execute_long_math(|a, b| Ok(a * b))?,
                Instruction::Ldiv => self.execute_long_math(|a, b| match b {
                    0 => Err(VmError::ArithmeticException),
                    _ => Ok(a / b),
                })?,
                Instruction::Lrem => self.execute_long_math(|a, b| match b {
                    0 => Err(VmError::ArithmeticException),
                    _ => Ok(a % b),
                })?,
                Instruction::Land => self.execute_long_math(|a, b| Ok(a & b))?,
                Instruction::Lor => self.execute_long_math(|a, b| Ok(a | b))?,
                Instruction::Lxor => self.execute_long_math(|a, b| Ok(a ^ b))?,
                Instruction::Lshr => self.execute_long_shift(|a, b| Ok(a >> (b & 0x1f)))?,
                Instruction::Lshl => self.execute_long_shift(|a, b| Ok(a << (b & 0x1f)))?,
                Instruction::Lushr => self.execute_long_shift(|a, b| {
                    Ok({
                        if a > 0 {
                            a >> (b & 0x1f)
                        } else {
                            ((a as u64) >> (b & 0x1f)) as i64
                        }
                    })
                })?,

                Instruction::Fadd => self.execute_float_math(|a, b| Ok(a + b))?,
                Instruction::Fsub => self.execute_float_math(|a, b| Ok(a - b))?,
                Instruction::Fmul => self.execute_float_math(|a, b| Ok(a * b))?,
                Instruction::Fdiv => self.execute_float_math(|a, b| {
                    Ok(
                        if Self::is_double_division_returning_nan(a as f64, b as f64) {
                            f32::NAN
                        } else {
                            a / b
                        },
                    )
                })?,
                Instruction::Frem => self.execute_float_math(|a, b| {
                    Ok(
                        if Self::is_double_division_returning_nan(a as f64, b as f64) {
                            f32::NAN
                        } else {
                            a % b
                        },
                    )
                })?,

                Instruction::Dadd => self.execute_double_math(|a, b| Ok(a + b))?,
                Instruction::Dsub => self.execute_double_math(|a, b| Ok(a - b))?,
                Instruction::Dmul => self.execute_double_math(|a, b| Ok(a * b))?,
                Instruction::Ddiv => self.execute_double_math(|a, b| {
                    Ok(if Self::is_double_division_returning_nan(a, b) {
                        f64::NAN
                    } else {
                        a / b
                    })
                })?,
                Instruction::Drem => self.execute_double_math(|a, b| {
                    Ok(if Self::is_double_division_returning_nan(a, b) {
                        f64::NAN
                    } else {
                        a % b
                    })
                })?,

                Instruction::Ineg => self.execute_ineg()?,
                Instruction::Lneg => self.execute_lneg()?,
                Instruction::Fneg => self.execute_fneg()?,
                Instruction::Dneg => self.execute_dneg()?,

                Instruction::Goto(jump_address) => self.goto(jump_address),

                Instruction::Ifeq(jump_address) => self.execute_if(jump_address, |v| v == 0)?,
                Instruction::Ifne(jump_address) => self.execute_if(jump_address, |v| v != 0)?,
                Instruction::Iflt(jump_address) => self.execute_if(jump_address, |v| v < 0)?,
                Instruction::Ifle(jump_address) => self.execute_if(jump_address, |v| v <= 0)?,
                Instruction::Ifgt(jump_address) => self.execute_if(jump_address, |v| v > 0)?,
                Instruction::Ifge(jump_address) => self.execute_if(jump_address, |v| v >= 0)?,
                Instruction::Ifnull(jump_address) => self.execute_if_null(jump_address, true)?,
                Instruction::Ifnonnull(jump_address) => {
                    self.execute_if_null(jump_address, false)?
                }

                Instruction::If_icmpeq(jump_address) => {
                    self.execute_if_icmp(jump_address, |a, b| a == b)?
                }
                Instruction::If_icmpne(jump_address) => {
                    self.execute_if_icmp(jump_address, |a, b| a != b)?
                }
                Instruction::If_icmplt(jump_address) => {
                    self.execute_if_icmp(jump_address, |a, b| a < b)?
                }
                Instruction::If_icmple(jump_address) => {
                    self.execute_if_icmp(jump_address, |a, b| a <= b)?
                }
                Instruction::If_icmpgt(jump_address) => {
                    self.execute_if_icmp(jump_address, |a, b| a > b)?
                }
                Instruction::If_icmpge(jump_address) => {
                    self.execute_if_icmp(jump_address, |a, b| a >= b)?
                }

                Instruction::Lcmp => self.execute_long_compare(1)?,
                Instruction::Fcmpg => self.execute_float_compare(-1)?,
                Instruction::Fcmpl => self.execute_float_compare(1)?,
                Instruction::Dcmpg => self.execute_double_compare(-1)?,
                Instruction::Dcmpl => self.execute_double_compare(1)?,

                Instruction::Newarray(array_type) => {
                    self.execute_newarray(array_type)?;
                }
                Instruction::Anewarray(constant_index) => {
                    self.execute_anewarray(constant_index)?;
                }

                Instruction::Arraylength => self.execute_array_length()?,

                Instruction::Baload => self.execute_baload()?,
                Instruction::Caload => self.execute_caload()?,
                Instruction::Saload => self.execute_saload()?,
                Instruction::Iaload => self.execute_iaload()?,
                Instruction::Laload => self.execute_laload()?,
                Instruction::Faload => self.execute_faload()?,
                Instruction::Daload => self.execute_daload()?,
                Instruction::Aaload => self.execute_aaload()?,

                Instruction::Bastore => self.execute_bastore()?,
                Instruction::Castore => self.execute_castore()?,
                Instruction::Sastore => self.execute_sastore()?,
                Instruction::Iastore => self.execute_iastore()?,
                Instruction::Lastore => self.execute_lastore()?,
                Instruction::Fastore => self.execute_fastore()?,
                Instruction::Dastore => self.execute_dastore()?,
                Instruction::Aastore => self.execute_aastore(vm)?,

                Instruction::Nop => {}

                _ => {
                    warn!("Unsupported instruction: {:?}", instruction);
                    return Err(VmError::NotImplemented);
                }
            }
        }
    }

    fn i2b(value: i32) -> Value<'a> {
        Int((value as i8) as i32)
    }
    fn i2c(value: i32) -> Value<'a> {
        Int((value as u16) as i32)
    }
    fn i2s(value: i32) -> Value<'a> {
        Int((value as i16) as i32)
    }
    fn i2i(value: i32) -> Value<'a> {
        Int(value)
    }
    fn i2f(value: i32) -> Value<'a> {
        Float(value as f32)
    }
    fn i2l(value: i32) -> Value<'a> {
        Long(value as i64)
    }
    fn i2d(value: i32) -> Value<'a> {
        Double(value as f64)
    }

    fn l2i(value: i64) -> Value<'a> {
        Int(value as i32)
    }
    fn l2f(value: i64) -> Value<'a> {
        Float(value as f32)
    }
    fn l2l(value: i64) -> Value<'a> {
        Long(value)
    }
    fn l2d(value: i64) -> Value<'a> {
        Double(value as f64)
    }

    fn f2i(value: f32) -> Value<'a> {
        Int(value as i32)
    }
    fn f2l(value: f32) -> Value<'a> {
        Long(value as i64)
    }
    fn f2f(value: f32) -> Value<'a> {
        Float(value)
    }
    fn f2d(value: f32) -> Value<'a> {
        Double(value as f64)
    }

    fn d2i(value: f64) -> Value<'a> {
        Int(value as i32)
    }
    fn d2f(value: f64) -> Value<'a> {
        Float(value as f32)
    }
    fn d2l(value: f64) -> Value<'a> {
        Long(value as i64)
    }
    fn d2d(value: f64) -> Value<'a> {
        Double(value)
    }

    fn invoke_method(
        &mut self,
        vm: &mut Vm<'a>,
        call_stack: &mut CallStack<'a>,
        constant_index: u16,
        kind: InvokeKind,
    ) -> Result<(), VmError> {
        let static_method_reference =
            self.get_method_to_invoke_statically(vm, constant_index, kind)?;
        let (receiver, params, new_stack_len) =
            self.get_method_receiver_and_params(&static_method_reference)?;
        let class_and_method = match kind {
            InvokeKind::Virtual => {
                Self::resolve_virtual_method(vm, receiver, static_method_reference)?
            }
            _ => static_method_reference,
        };
        self.stack.truncate(new_stack_len)?;

        let method_return_type = class_and_method.return_type();
        let result = vm.invoke(call_stack, class_and_method, receiver, params)?;

        Self::validate_type_opt(vm, method_return_type, &result)?;
        if let Some(value) = result {
            self.stack.push(value)?;
        }
        Ok(())
    }

    fn get_field(
        class: &'a Class,
        field_reference: FieldReference,
    ) -> Result<(usize, &'a ClassFileField), VmError> {
        class
            .find_field(field_reference.class_name, field_reference.field_name)
            .ok_or(VmError::FieldNotFoundException(
                field_reference.class_name.to_string(),
                field_reference.field_name.to_string(),
            ))
    }

    generate_pop!(pop_int, Int, i32);
    generate_pop!(pop_long, Long, i64);
    generate_pop!(pop_float, Float, f32);
    generate_pop!(pop_double, Double, f64);
    generate_pop!(pop_object, Object, ObjectRef<'a>);

    fn pop_array(&mut self) -> Result<(FieldType, ArrayRef<'a>), VmError> {
        let receiver = self.stack.pop()?;
        match receiver {
            Array(field_type, vector) => Ok((field_type, vector)),
            _ => Err(VmError::ValidationException),
        }
    }

    fn get_constant(&self, constant_index: u16) -> Result<&ConstantPoolEntry, VmError> {
        self.class_and_method
            .class
            .constants
            .get(constant_index)
            .map_err(|_| VmError::ValidationException)
    }

    fn get_constant_class_reference(&self, constant_index: u16) -> Result<&str, VmError> {
        let constant = self.get_constant(constant_index)?;
        if let &ConstantPoolEntry::ClassReference(constant_index) = constant {
            self.get_constant_utf8(constant_index)
        } else {
            Err(VmError::ValidationException)
        }
    }

    fn get_constant_method_reference(
        &self,
        constant_index: u16,
    ) -> Result<MethodReference, VmError> {
        let constant = self.get_constant(constant_index)?;
        if let &ConstantPoolEntry::MethodReference(
            class_name_index,
            name_and_type_descriptor_index,
        ) = constant
        {
            let class_name = self.get_constant_class_reference(class_name_index)?;
            let constant = self.get_constant(name_and_type_descriptor_index)?;
            if let &ConstantPoolEntry::NameAndTypeDescriptor(name_index, type_descriptor_index) =
                constant
            {
                let method_name = self.get_constant_utf8(name_index)?;
                let type_descriptor = self.get_constant_utf8(type_descriptor_index)?;
                return Ok(MethodReference {
                    class_name,
                    method_name,
                    type_descriptor,
                });
            }
        }
        Err(VmError::ValidationException)
    }

    fn get_constant_field_reference(&self, constant_index: u16) -> Result<FieldReference, VmError> {
        let constant = self.get_constant(constant_index)?;
        if let &ConstantPoolEntry::FieldReference(
            class_name_index,
            name_and_type_descriptor_index,
        ) = constant
        {
            let class_name = self.get_constant_class_reference(class_name_index)?;
            let constant = self.get_constant(name_and_type_descriptor_index)?;
            if let &ConstantPoolEntry::NameAndTypeDescriptor(name_index, type_descriptor_index) =
                constant
            {
                let field_name = self.get_constant_utf8(name_index)?;
                let type_descriptor = self.get_constant_utf8(type_descriptor_index)?;
                return Ok(FieldReference {
                    class_name,
                    field_name,
                    type_descriptor,
                });
            }
        }
        Err(VmError::ValidationException)
    }

    fn get_constant_utf8(&self, constant_index: u16) -> Result<&str, VmError> {
        let constant = self.get_constant(constant_index)?;
        if let ConstantPoolEntry::Utf8(string) = constant {
            Ok(string)
        } else {
            Err(VmError::ValidationException)
        }
    }

    fn get_method_to_invoke_statically(
        &self,
        vm: &mut Vm<'a>,
        constant_index: u16,
        kind: InvokeKind,
    ) -> Result<ClassAndMethod<'a>, VmError> {
        let method_reference = self.get_constant_method_reference(constant_index)?;

        let class = vm.get_or_resolve_class(method_reference.class_name)?;
        match kind {
            InvokeKind::Special | InvokeKind::Static => {
                Self::get_method_of_class(class, method_reference)
                    .map(|method| ClassAndMethod { class, method })
            }
            InvokeKind::Virtual => Self::get_method_checking_superclasses(class, method_reference),
        }
    }

    fn get_method_of_class<'b>(
        class: &'b Class<'a>,
        method_reference: MethodReference,
    ) -> Result<&'b ClassFileMethod, VmError> {
        class
            .find_method(
                method_reference.method_name,
                method_reference.type_descriptor,
            )
            .ok_or(VmError::MethodNotFoundException(
                class.name.to_string(),
                method_reference.method_name.to_string(),
                method_reference.type_descriptor.to_string(),
            ))
    }

    fn get_method_checking_superclasses<'b>(
        class: &'b Class<'a>,
        method_reference: MethodReference,
    ) -> Result<ClassAndMethod<'b>, VmError> {
        let mut curr_class = class;
        loop {
            if let Some(method) = curr_class.find_method(
                method_reference.method_name,
                method_reference.type_descriptor,
            ) {
                return Ok(ClassAndMethod {
                    class: curr_class,
                    method,
                });
            }

            if let Some(superclass) = class.superclass {
                curr_class = superclass;
            } else {
                return Err(VmError::MethodNotFoundException(
                    class.name.to_string(),
                    method_reference.method_name.to_string(),
                    method_reference.type_descriptor.to_string(),
                ));
            }
        }
    }

    fn resolve_virtual_method(
        vm: &Vm<'a>,
        receiver: Option<ObjectRef>,
        class_and_method: ClassAndMethod,
    ) -> Result<ClassAndMethod<'a>, VmError> {
        match receiver {
            None => Err(VmError::ValidationException),
            Some(receiver) => {
                let receiver_class = vm.find_class_by_id(receiver.class_id).ok_or(
                    VmError::ClassNotFoundException(receiver.class_id.to_string()),
                )?;
                let resolved_method = Self::get_method_checking_superclasses(
                    receiver_class,
                    MethodReference {
                        class_name: &class_and_method.class.name,
                        method_name: &class_and_method.method.name,
                        type_descriptor: &class_and_method.method.type_descriptor,
                    },
                )?;
                debug!(
                    "resolved virtual method {}.{}:{} on object of class {}: using version of class {}",
                    class_and_method.class.name,
                    class_and_method.method.name,
                    class_and_method.method.type_descriptor,
                    receiver_class.name,
                    resolved_method.class.name,
                );
                Ok(resolved_method)
            }
        }
    }

    fn get_method_receiver_and_params(
        &self,
        class_and_method: &ClassAndMethod<'a>,
    ) -> Result<(Option<ObjectRef<'a>>, Vec<Value<'a>>, usize), VmError> {
        let cur_stack_len = self.stack.len();
        let receiver_count = if class_and_method.is_static() { 0 } else { 1 };
        let num_params = class_and_method.num_arguments();
        if cur_stack_len < (receiver_count + num_params) {
            return Err(VmError::ValidationException);
        }

        let receiver = if class_and_method.is_static() {
            None
        } else {
            Some(self.get_object_from_stack(
                cur_stack_len - num_params - receiver_count,
                class_and_method.class,
            )?)
        };
        let mut params = Vec::from(&self.stack[cur_stack_len - num_params..cur_stack_len]);
        Self::fix_long_and_double_params(&mut params)?;
        Ok((
            receiver,
            params,
            cur_stack_len - num_params - receiver_count,
        ))
    }

    // long and double arguments should take two slots in the variable table
    // Since in our implementation we do not "split" the numbers in two 32-bits parts,
    // we can just add an empty slot in the variable after a long or a double. All the
    // bytecode instructions should refer to the "first" value anyway.
    fn fix_long_and_double_params(params: &mut Vec<Value>) -> Result<(), VmError> {
        let mut num_params = params.len();
        let mut i = 0usize;
        while i < num_params {
            let val = &params[i];
            match val {
                Long(_) | Double(_) => {
                    params.insert(i + 1, Value::Uninitialized);
                    num_params += 1;
                }
                _ => {}
            }
            i += 1;
        }
        Ok(())
    }

    fn get_object_from_stack(
        &self,
        index: usize,
        _expected_class: &Class,
    ) -> Result<ObjectRef<'a>, VmError> {
        let receiver = self.stack.get(index).ok_or(VmError::ValidationException)?;
        match receiver {
            Object(object) => {
                // TODO: here we should check "instanceof" the expected class of a subclass
                Ok(object)
            }
            _ => Err(VmError::ValidationException),
        }
    }

    fn validate_type_opt(
        vm: &Vm,
        expected_type: Option<FieldType>,
        value: &Option<Value<'a>>,
    ) -> Result<(), VmError> {
        match expected_type {
            None => match value {
                None => Ok(()),
                Some(_) => Err(VmError::ValidationException),
            },
            Some(expected_type) => match value {
                None => Err(VmError::ValidationException),
                Some(value) => Self::validate_type(vm, expected_type, value),
            },
        }
    }

    fn validate_type(vm: &Vm, expected_type: FieldType, value: &Value) -> Result<(), VmError> {
        if value.matches_type(expected_type, |class_id| vm.find_class_by_id(class_id)) {
            Ok(())
        } else {
            Err(VmError::ValidationException)
        }
    }

    generate_execute_return!(execute_ireturn, Int);
    generate_execute_return!(execute_lreturn, Long);
    generate_execute_return!(execute_freturn, Float);
    generate_execute_return!(execute_dreturn, Double);

    fn get_local_int(&self, vm: &Vm, index: usize) -> Result<Value<'a>, VmError> {
        let variable = self.locals.get(index).ok_or(VmError::ValidationException)?;
        Self::validate_type(vm, Base(BaseType::Int), variable)?;
        Ok(variable.clone())
    }

    fn get_local_int_as_int(&self, vm: &Vm, index: usize) -> Result<i32, VmError> {
        let value = self.get_local_int(vm, index)?;
        match value {
            Int(the_int) => Ok(the_int),
            _ => Err(VmError::ValidationException),
        }
    }

    generate_execute_math!(execute_int_math, pop_int, Int, i32);
    generate_execute_math!(execute_long_math, pop_long, Long, i64);
    generate_execute_math!(execute_float_math, pop_float, Float, f32);
    generate_execute_math!(execute_double_math, pop_double, Double, f64);

    fn execute_long_shift<T>(&mut self, evaluator: T) -> Result<(), VmError>
    where
        T: FnOnce(i64, i32) -> Result<i64, VmError>,
    {
        let val2 = self.pop_int()?;
        let val1 = self.pop_long()?;
        let result = evaluator(val1, val2)?;
        self.stack.push(Long(result))
    }

    fn is_double_division_returning_nan(a: f64, b: f64) -> bool {
        a.is_nan()
            || b.is_nan()
            || (a.is_infinite() && b.is_infinite())
            || ((a == 0f64 || a == -0f64) && (b == 0f64 || b == -0f64))
    }

    generate_execute_neg!(execute_ineg, pop_int, Int);
    generate_execute_neg!(execute_lneg, pop_long, Long);
    generate_execute_neg!(execute_fneg, pop_float, Float);
    generate_execute_neg!(execute_dneg, pop_double, Double);

    generate_execute_coerce!(coerce_int, pop_int, i32);
    generate_execute_coerce!(coerce_long, pop_long, i64);
    generate_execute_coerce!(coerce_float, pop_float, f32);
    generate_execute_coerce!(coerce_double, pop_double, f64);

    fn goto(&mut self, jump_address: u16) {
        self.pc = jump_address.into_usize_safe();
    }

    fn execute_if<T>(&mut self, jump_address: u16, comparator: T) -> Result<(), VmError>
    where
        T: FnOnce(i32) -> bool,
    {
        let value = self.pop_int()?;
        if comparator(value) {
            self.goto(jump_address);
        }
        Ok(())
    }

    fn execute_if_icmp<T>(&mut self, jump_address: u16, comparator: T) -> Result<(), VmError>
    where
        T: FnOnce(i32, i32) -> bool,
    {
        let val2 = self.pop_int()?;
        let val1 = self.pop_int()?;
        if comparator(val1, val2) {
            self.goto(jump_address);
        }
        Ok(())
    }

    fn execute_if_null(&mut self, jump_address: u16, jump_on_null: bool) -> Result<(), VmError> {
        let value = self.stack.pop()?;
        match value {
            Object(_) => {
                if !jump_on_null {
                    self.goto(jump_address);
                }
            }
            Value::Null => {
                if jump_on_null {
                    self.goto(jump_address);
                }
            }
            _ => return Err(VmError::ValidationException),
        }
        Ok(())
    }

    generate_compare!(execute_long_compare, pop_long);
    generate_compare!(execute_float_compare, pop_float);
    generate_compare!(execute_double_compare, pop_double);

    generate_execute_load!(execute_aload, Object, Array);
    generate_execute_load!(execute_iload, Int);
    generate_execute_load!(execute_lload, Long);
    generate_execute_load!(execute_fload, Float);
    generate_execute_load!(execute_dload, Double);

    generate_execute_store!(execute_astore, Object, Array);
    generate_execute_store!(execute_istore, Int);
    generate_execute_store!(execute_lstore, Long);
    generate_execute_store!(execute_fstore, Float);
    generate_execute_store!(execute_dstore, Double);

    fn execute_ldc(&mut self, vm: &mut Vm<'a>, index: u16) -> Result<(), VmError> {
        let constant_value = self.get_constant(index)?;
        match constant_value {
            ConstantPoolEntry::Integer(value) => self.stack.push(Int(*value)),
            ConstantPoolEntry::Float(value) => self.stack.push(Float(*value)),
            ConstantPoolEntry::StringReference(string_index) => {
                let constant = self.get_constant(*string_index)?;
                match constant {
                    ConstantPoolEntry::Utf8(string) => {
                        let char_array: Vec<Value<'a>> =
                            string.encode_utf16().map(|c| Int(c as i32)).collect();
                        let char_array = Rc::new(RefCell::new(char_array));
                        let char_array = Array(Base(BaseType::Char), char_array);

                        // TODO: static fields
                        // In our JRE's rt.jar, the fields for String are:
                        //    private final char[] value;
                        //    private int hash;
                        //    private static final long serialVersionUID = -6849794470754667710L;
                        //    private static final ObjectStreamField[] serialPersistentFields = new ObjectStreamField[0];
                        //    public static final Comparator<String> CASE_INSENSITIVE_ORDER = new CaseInsensitiveComparator();
                        //    private static final int HASHING_SEED;
                        //    private transient int hash32;
                        let string_object = vm.new_object("java/lang/String")?;
                        string_object.set_field(0, char_array);
                        string_object.set_field(1, Int(0));
                        string_object.set_field(6, Int(0));

                        self.stack.push(Object(string_object))
                    }
                    _ => Err(VmError::ValidationException),
                }
            }
            // TODO: StringReference
            // TODO: ClassReference
            // TODO: method type or method handle
            _ => Err(VmError::ValidationException),
        }
    }

    fn execute_ldc_long_double(&mut self, index: u16) -> Result<(), VmError> {
        let constant_value = self.get_constant(index)?;
        match constant_value {
            ConstantPoolEntry::Long(value) => self.stack.push(Long(*value)),
            ConstantPoolEntry::Double(value) => self.stack.push(Double(*value)),
            _ => Err(VmError::ValidationException),
        }
    }

    fn execute_newarray(&mut self, array_type: NewArrayType) -> Result<(), VmError> {
        let length = self.pop_int()?.into_usize_safe();

        let (elements_type, default_value) = match array_type {
            NewArrayType::Boolean => (Base(BaseType::Boolean), Int(0)),
            NewArrayType::Char => (Base(BaseType::Char), Int(0)),
            NewArrayType::Float => (Base(BaseType::Float), Float(0f32)),
            NewArrayType::Double => (Base(BaseType::Double), Double(0f64)),
            NewArrayType::Byte => (Base(BaseType::Byte), Int(0)),
            NewArrayType::Short => (Base(BaseType::Short), Int(0)),
            NewArrayType::Int => (Base(BaseType::Int), Int(0)),
            NewArrayType::Long => (Base(BaseType::Long), Long(0)),
        };

        let vec = vec![default_value; length];
        let vec = Rc::new(RefCell::new(vec));
        let array_value = Array(elements_type, vec);
        self.stack.push(array_value)
    }

    fn execute_anewarray(&mut self, constant_index: u16) -> Result<(), VmError> {
        let length = self.pop_int()?.into_usize_safe();
        let class_name = self.get_constant_class_reference(constant_index)?;

        let vec = vec![Value::Null; length];
        let vec = Rc::new(RefCell::new(vec));
        let array_value = Array(FieldType::Object(class_name.to_string()), vec);
        self.stack.push(array_value)
    }

    fn execute_array_length(&mut self) -> Result<(), VmError> {
        let (_, array) = self.pop_array()?;
        self.stack.push(Int(array.borrow().len() as i32))?;
        Ok(())
    }

    generate_execute_array_load!(
        execute_baload,
        Base(BaseType::Byte),
        Base(BaseType::Boolean)
    );
    generate_execute_array_load!(execute_caload, Base(BaseType::Char));
    generate_execute_array_load!(execute_saload, Base(BaseType::Short));
    generate_execute_array_load!(execute_iaload, Base(BaseType::Int));
    generate_execute_array_load!(execute_laload, Base(BaseType::Long));
    generate_execute_array_load!(execute_faload, Base(BaseType::Float));
    generate_execute_array_load!(execute_daload, Base(BaseType::Double));
    generate_execute_array_load!(execute_aaload, FieldType::Object(_));

    generate_execute_array_store!(
        execute_bastore,
        pop_int,
        i2b,
        Base(BaseType::Byte),
        Base(BaseType::Boolean)
    );
    generate_execute_array_store!(execute_castore, pop_int, i2c, Base(BaseType::Char));
    generate_execute_array_store!(execute_sastore, pop_int, i2s, Base(BaseType::Short));
    generate_execute_array_store!(execute_iastore, pop_int, i2i, Base(BaseType::Int));
    generate_execute_array_store!(execute_lastore, pop_long, l2l, Base(BaseType::Long));
    generate_execute_array_store!(execute_fastore, pop_float, f2f, Base(BaseType::Float));
    generate_execute_array_store!(execute_dastore, pop_double, d2d, Base(BaseType::Double));

    fn execute_aastore(&mut self, vm: &Vm) -> Result<(), VmError> {
        let value = Object(self.pop_object()?);
        let index = self.pop_int()?.into_usize_safe();
        let (field_type, array) = self.pop_array()?;
        match field_type {
            FieldType::Object(array_type) => {
                Self::validate_type(vm, FieldType::Object(array_type), &value)?;
                match array.borrow_mut().get_mut(index) {
                    None => return Err(VmError::ArrayIndexOutOfBoundsException),
                    Some(reference) => *reference = value,
                }
            }
            _ => return Err(VmError::ValidationException),
        }
        Ok(())
    }

    fn debug_start_execution(&self) {
        debug!(
            "starting execution of method {}::{} - locals are {:?}",
            self.class_and_method.class.name, self.class_and_method.method.name, self.locals
        )
    }

    fn debug_print_status(&self, instruction: &Instruction) {
        debug!("FRAME STATUS: pc: {}", self.pc);
        debug!("  stack:");
        for stack_entry in self.stack.iter() {
            debug!("  - {:?}", stack_entry);
        }
        debug!("  locals:");
        for local_variable in self.locals.iter() {
            debug!("  - {:?}", local_variable);
        }
        debug!("  next instruction: {:?}", instruction)
    }

    fn debug_done_execution(&self, result: Option<&Value>) {
        debug!(
            "completed execution of method {}::{} - result is {:?}",
            self.class_and_method.class.name, self.class_and_method.method.name, result
        )
    }
}
