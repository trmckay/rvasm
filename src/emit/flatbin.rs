use crate::arch;
use crate::parser::Node;
use smallvec::SmallVec;
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub enum EmitError {
    UnexpectedNodeType(String),
    InvalidInstruction(String),
    InvalidArgumentCount(String),
    InvalidArgumentType(String, usize),
    InvalidEncoding(String),
    DuplicateLabel(String),
    DuplicateConstant(String),
}

pub fn emit_flat_binary(spec: &arch::RiscVSpec, ast: &Node) -> Result<Vec<u8>, EmitError> {
    let mut state = BinaryEmitState {
        out_buf: Vec::new(),
        out_pos: 0,
        deferred: Vec::new(),
        label_set: HashMap::new(),
        local_label_set: HashMap::new(),
        const_set: HashMap::new(),
    };
    emit_binary_recurse(spec, &mut state, ast).map(move |_| state.out_buf)
}

#[derive(Debug)]
struct BinaryEmitState {
    out_buf: Vec<u8>,
    out_pos: usize,
    deferred: Vec<(usize, Node)>,
    label_set: HashMap<String, u64>,
    local_label_set: HashMap<String, u64>,
    const_set: HashMap<String, u64>,
}

impl BinaryEmitState {
    fn accomodate_bytes(&mut self, byte_count: usize) -> &mut [u8] {
        let start_pos = self.out_pos;
        let end_pos = start_pos + byte_count;
        if self.out_buf.len() < end_pos {
            self.out_buf.resize(end_pos, 0);
        }
        self.out_pos = end_pos;
        &mut self.out_buf[start_pos..end_pos]
    }

    fn find_const(&self, key: &str, spec: &arch::RiscVSpec) -> Option<u64> {
        self.label_set
            .get(key)
            .or_else(|| self.local_label_set.get(key))
            .or_else(|| self.const_set.get(key))
            .copied()
            .or_else(|| spec.get_const(key))
    }
}

fn emit_deferred(spec: &arch::RiscVSpec, state: &mut BinaryEmitState) -> Result<(), EmitError> {
    let mut to_remove = Vec::new();
    let mut to_emit = Vec::new();
    for (i, (pos, insn)) in state.deferred.iter().enumerate() {
        let pc = *pos as u64;
        let simp = insn.emitter_simplify(&|cname| state.find_const(cname, spec), pc);
        if !simp.1 {
            continue;
        }
        to_emit.push((*pos, simp.0));
        to_remove.push(i);
    }
    for i in to_remove.iter().rev() {
        state.deferred.swap_remove(*i);
    }
    for (pos, insn) in to_emit.into_iter() {
        let saved_pos = state.out_pos;
        state.out_pos = pos;
        emit_binary_recurse(&spec, state, &insn)?;
        state.out_pos = saved_pos;
    }
    Ok(())
}

fn emit_binary_recurse(
    spec: &arch::RiscVSpec,
    state: &mut BinaryEmitState,
    node: &Node,
) -> Result<(), EmitError> {
    use Node::*;

    let ialign_bytes = (spec.get_const("IALIGN").unwrap_or(32) as usize + 7) / 8;
    let max_ilen_bytes = (spec.get_const("ILEN").unwrap_or(32) as usize + 7) / 8;

    match node {
        Root(nodes) => {
            for node in nodes.iter() {
                emit_binary_recurse(spec, state, node)?;
            }
            emit_deferred(spec, state)?;
            if let Some(defnode) = state.deferred.first() {
                return Err(EmitError::UnexpectedNodeType(format!("{:?}", defnode)));
            }
            Ok(())
        }
        Label(lname) => {
            if lname.starts_with('.') {
                // local label
                if state
                    .local_label_set
                    .insert(lname.to_owned(), state.out_pos as u64)
                    .is_some()
                {
                    return Err(EmitError::DuplicateLabel(lname.to_owned()));
                }
            } else {
                // handle all previous labels and local labels
                emit_deferred(spec, state)?;
                state.local_label_set.clear();

                if state
                    .label_set
                    .insert(lname.to_owned(), state.out_pos as u64)
                    .is_some()
                {
                    return Err(EmitError::DuplicateLabel(lname.to_owned()));
                }
            }
            Ok(())
        }
        Instruction(iname, args) => {
            match iname.as_ref() {
                // .org ADDRESS
                ".org" | ".ORG" => {
                    if args.len() != 1 {
                        return Err(EmitError::InvalidArgumentCount(iname.clone()));
                    }
                    if let (Node::Argument(box Node::Integer(adr)), _) = args[0].emitter_simplify(
                        &|cname| state.find_const(cname, spec),
                        state.out_pos as u64,
                    ) {
                        let new_out_pos = adr as usize;
                        if new_out_pos > state.out_buf.len() {
                            state
                                .out_buf
                                .reserve(new_out_pos - state.out_buf.len() + 32 * 32);
                            state.out_buf.resize(new_out_pos, 0);
                        }
                        state.out_pos = new_out_pos;
                        Ok(())
                    } else {
                        Err(EmitError::InvalidArgumentType(iname.clone(), 0))
                    }
                }
                // .equ/.define NAME VALUE
                ".equ" | ".EQU" | ".define" | ".DEFINE" => {
                    if args.len() != 2 {
                        return Err(EmitError::InvalidArgumentCount(iname.clone()));
                    }
                    if let Node::Argument(box Node::Identifier(defname)) = &args[0] {
                        if let (Node::Argument(box Node::Integer(val)), _) = args[1]
                            .emitter_simplify(
                                &|cname| state.find_const(cname, spec),
                                state.out_pos as u64,
                            )
                        {
                            if state.const_set.insert(defname.to_owned(), val).is_none() {
                                Ok(())
                            } else {
                                Err(EmitError::DuplicateConstant(defname.to_owned()))
                            }
                        } else {
                            Err(EmitError::InvalidArgumentType(iname.clone(), 1))
                        }
                    } else {
                        Err(EmitError::InvalidArgumentType(iname.clone(), 0))
                    }
                }
                // Standard RISC-V instructions
                _ => {
                    // check spec
                    let specinsn = spec
                        .get_instruction_by_name(iname)
                        .ok_or_else(|| EmitError::InvalidInstruction(iname.clone()))?;
                    let fmt = specinsn.get_format(&spec);
                    if args.len() != specinsn.args.len() {
                        return Err(EmitError::InvalidArgumentCount(iname.clone()));
                    }

                    // check length
                    let ilen_bytes = (fmt.ilen + 7) / 8;
                    if ilen_bytes > max_ilen_bytes {
                        return Err(EmitError::InvalidEncoding(iname.clone()));
                    }
                    // check alignment
                    let aligned_pos =
                        (state.out_pos + ialign_bytes - 1) / ialign_bytes * ialign_bytes;
                    if state.out_pos != aligned_pos {
                        // pad out with zeroes
                        // TODO: NOP alignment instead of zero alignment
                        state.accomodate_bytes(aligned_pos - state.out_pos);
                    }

                    // simplify and defer if necessary
                    let simpinsn = node.emitter_simplify(
                        &|cname| state.find_const(cname, spec),
                        state.out_pos as u64,
                    );
                    if !simpinsn.1 {
                        state.deferred.push((state.out_pos, simpinsn.0));
                        state.accomodate_bytes(ilen_bytes);
                        return Ok(());
                    }
                    let args;
                    if let Node::Instruction(_, sargs) = simpinsn.0 {
                        args = sargs;
                    } else {
                        panic!("Simplified instruction is now a {:?}", simpinsn.0);
                    }

                    // handle arguments
                    let mut argv: SmallVec<[u64; 4]> = SmallVec::new();
                    for (i, arg) in args.iter().enumerate() {
                        match fmt.fields[specinsn.args[i]].vtype {
                            arch::FieldType::Value => {
                                if let Node::Argument(box Node::Integer(val)) = arg {
                                    argv.push(*val);
                                } else {
                                    return Err(EmitError::InvalidArgumentType(iname.clone(), i));
                                }
                            }
                            arch::FieldType::Register => {
                                if let Node::Argument(box Node::Register(rid)) = arg {
                                    argv.push(*rid as u64);
                                } else {
                                    return Err(EmitError::InvalidArgumentType(iname.clone(), i));
                                }
                            }
                        }
                    }
                    assert_eq!(argv.len(), specinsn.args.len());

                    // emit instruction
                    let bytes = state.accomodate_bytes(ilen_bytes);
                    specinsn
                        .encode_into(bytes, spec, argv.as_slice())
                        .map_err(|_| EmitError::InvalidEncoding(iname.clone()))
                }
            }
        }
        _ => Err(EmitError::UnexpectedNodeType(format!("{:?}", node))),
    }
}
