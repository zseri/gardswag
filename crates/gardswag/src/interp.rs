use crossbeam_utils::thread;
use gardswag_syntax::{self as synt, Block, Expr};
use gardswag_varstack::VarStack;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub enum Builtin {
    Plus,
    Minus,
    Mult,
    Eq,
    Leq,
    Not,
    SpawnThread,
    MakeChan,
    ChanSend,
    ChanRecv,
    StdioWrite,
}

impl Builtin {
    fn argc(self) -> usize {
        match self {
            Self::Plus | Self::Minus | Self::Mult | Self::Eq | Self::Leq => 2,
            Self::Not | Self::SpawnThread | Self::StdioWrite => 1,
            Self::MakeChan => 1,
            Self::ChanSend => 2,
            Self::ChanRecv => 1,
        }
    }
}

#[derive(Clone, Debug)]
#[must_use = "the interpreter shouldn't blindly discard values"]
pub enum Value<'a> {
    Unit,
    Boolean(bool),
    Integer(i32),
    PureString(String),

    Record(BTreeMap<&'a str, Value<'a>>),

    Tagger {
        key: &'a str,
    },

    Tagged {
        key: &'a str,
        value: Box<Value<'a>>,
    },

    Builtin {
        f: Builtin,
        args: Vec<Value<'a>>,
    },
    Lambda {
        argname: &'a str,
        f: &'a Expr,
        stacksave: BTreeMap<String, Value<'a>>,
    },
    FixLambda {
        argname: &'a str,
        f: &'a Expr,
    },

    ChanSend(crossbeam_channel::Sender<Value<'a>>),
    ChanRecv(crossbeam_channel::Receiver<Value<'a>>),
}

impl<'a> core::cmp::PartialEq for Value<'a> {
    fn eq(&self, oth: &Self) -> bool {
        use Value as V;
        match (self, oth) {
            (V::Unit, V::Unit) => true,
            (V::Boolean(a), V::Boolean(b)) => a == b,
            (V::Integer(a), V::Integer(b)) => a == b,
            (V::PureString(a), V::PureString(b)) => a == b,
            (V::Record(a), V::Record(b)) => a == b,
            (V::Tagger { key: a }, V::Tagger { key: b }) => a == b,
            (V::Tagged { key: ka, value: va }, V::Tagged { key: kb, value: vb }) => {
                ka == kb && va == vb
            }
            (V::Builtin { f: fa, args: aa }, V::Builtin { f: fb, args: ab }) => {
                fa == fb && aa == ab
            }
            (
                V::Lambda {
                    argname: aa,
                    f: fa,
                    stacksave: sa,
                },
                V::Lambda {
                    argname: ab,
                    f: fb,
                    stacksave: sb,
                },
            ) => aa == ab && fa == fb && sa == sb,
            (V::FixLambda { argname: aa, f: fa }, V::FixLambda { argname: ab, f: fb }) => {
                aa == ab && fa == fb
            }
            (_, _) => false,
        }
    }
}

impl<'a> From<Builtin> for Value<'a> {
    fn from(x: Builtin) -> Self {
        Value::Builtin {
            f: x,
            args: Vec::new(),
        }
    }
}

#[derive(Clone, Copy)]
pub struct Env<'envout, 'envin> {
    pub thscope: &'envout thread::Scope<'envin>,
}

pub fn run_block<'a: 'envout + 'envin, 'envout, 'envin, 's>(
    env: Env<'envout, 'envin>,
    blk: &'a Block,
    stack: &'s VarStack<'s, Value<'a>>,
) -> Value<'a> {
    for i in &blk.stmts {
        let _ = run(env, i, stack);
    }
    if let Some(i) = &blk.term {
        run(env, i, stack)
    } else {
        Value::Unit
    }
}

fn run_stacksave<'a: 'envout + 'envin, 'envout, 'envin, 's, 's3, I, S>(
    env: Env<'envout, 'envin>,
    expr: &'a Expr,
    stack: &'s VarStack<'s, Value<'a>>,
    mut stacksave: I,
) -> Value<'a>
where
    I: Iterator<Item = (S, Value<'a>)>,
    S: AsRef<str>,
{
    match stacksave.next() {
        Some((name, value)) => run_stacksave(
            env,
            expr,
            &VarStack {
                parent: Some(stack),
                name: name.as_ref(),
                value,
            },
            stacksave,
        ),
        None => run(env, expr, stack),
    }
}

/// this function has a difficult signature,
/// because we really want to avoid all unnecessary allocations,
/// because it otherwise would be prohibitively costly...
fn run_pat<'a, 'b>(
    coll: &mut BTreeMap<&'a str, &'b Value<'a>>,
    pat: &'a synt::Pattern,
    inp: &'b Value<'a>,
) -> Option<()> {
    tracing::trace!("pat {:?}", pat);

    use synt::Pattern;

    match pat {
        Pattern::Identifier(i) => {
            coll.insert(&*i.inner, inp);
            Some(())
        }
        Pattern::Tagged { key, value } => match inp {
            Value::Tagged {
                key: got_key,
                value: got_value,
            } if key.inner == *got_key => run_pat(coll, value, got_value),
            _ => None,
        },
        Pattern::Record(synt::Offsetted { inner: rcpat, .. }) => match inp {
            Value::Record(rcm) if rcpat.len() <= rcm.len() => {
                for (key, value) in rcpat {
                    let got_value = rcm.get(&**key)?;
                    run_pat(coll, value, got_value)?;
                }
                Some(())
            }
            _ => None,
        },
        Pattern::Unit => match inp {
            Value::Unit => Some(()),
            _ => panic!("match on unit got unexpected value {:?}", inp),
        },
    }
}

pub fn run<'a: 'envout + 'envin, 'envout, 'envin, 's>(
    env: Env<'envout, 'envin>,
    expr: &'a Expr,
    stack: &'s VarStack<'s, Value<'a>>,
) -> Value<'a> {
    tracing::debug!("expr@{} : {}", expr.offset, expr.inner.typ());
    tracing::trace!("stack={:?}", stack);
    use gardswag_syntax::ExprKind as Ek;
    let res = match &expr.inner {
        Ek::Let { lhs, rhs, rest } => {
            let v_rhs = run(env, rhs, stack);
            run_block(
                env,
                rest,
                &VarStack {
                    parent: Some(stack),
                    name: &lhs.inner,
                    value: v_rhs,
                },
            )
        }
        Ek::Block(blk) => run_block(env, blk, stack),
        Ek::If {
            cond,
            then,
            or_else,
        } => {
            let v_cond = match run(env, cond, stack) {
                Value::Boolean(x) => x,
                x => panic!("invalid if condition: {:?}", x),
            };
            run_block(env, if v_cond { then } else { or_else }, stack)
        }
        Ek::Lambda { arg, body } => {
            let mut stacksave = std::collections::BTreeMap::new();
            for (k, v) in stack.iter() {
                if stacksave.contains_key(k) || k == arg || !body.inner.is_var_accessed(k) {
                    continue;
                }
                stacksave.insert(k.to_string(), v.clone());
            }

            Value::Lambda {
                argname: arg,
                f: body,
                stacksave,
            }
        }
        Ek::Call { prim, arg } => {
            let v_arg = run(env, arg, stack);
            let v_prim = run(env, prim, stack);
            match v_prim {
                Value::Builtin { f, mut args } => {
                    args.push(v_arg);
                    if f.argc() > args.len() {
                        Value::Builtin { f, args }
                    } else {
                        assert_eq!(f.argc(), args.len());
                        use Builtin as Bi;
                        match f {
                            Bi::Plus => match (args.get(0).unwrap(), args.get(1).unwrap()) {
                                (Value::Integer(a), Value::Integer(b)) => Value::Integer(*a + *b),
                                _ => panic!("std.plus called with {:?}", args),
                            },
                            Bi::Minus => match (args.get(0).unwrap(), args.get(1).unwrap()) {
                                (Value::Integer(a), Value::Integer(b)) => Value::Integer(*a - *b),
                                _ => panic!("std.minus called with {:?}", args),
                            },
                            Bi::Mult => match (args.get(0).unwrap(), args.get(1).unwrap()) {
                                (Value::Integer(a), Value::Integer(b)) => Value::Integer(*a * *b),
                                _ => panic!("std.minus called with {:?}", args),
                            },
                            Bi::Leq => match (args.get(0).unwrap(), args.get(1).unwrap()) {
                                (Value::Integer(a), Value::Integer(b)) => Value::Boolean(*a <= *b),
                                _ => panic!("std.minus called with {:?}", args),
                            },
                            Bi::Eq => Value::Boolean(args.get(0) == args.get(1)),
                            Bi::Not => match args.get(0).unwrap() {
                                Value::Boolean(b) => Value::Boolean(!*b),
                                a => panic!("std.not called with {:?}", a),
                            },
                            Bi::SpawnThread => {
                                let arg = args.pop().unwrap();
                                match arg {
                                    Value::Lambda {
                                        argname,
                                        f,
                                        stacksave,
                                    } => {
                                        env.thscope.spawn(move |thscope| {
                                            // luckily, we can rely on stacksave here
                                            match run_stacksave(
                                                Env { thscope },
                                                f,
                                                &VarStack {
                                                    parent: None,
                                                    name: argname,
                                                    value: Value::Unit,
                                                },
                                                stacksave.into_iter(),
                                            ) {
                                                Value::Unit => {}
                                                x => panic!(
                                                    "std.spawn_thread worker lambda returned {:?}",
                                                    x
                                                ),
                                            }
                                        });
                                        Value::Unit
                                    }
                                    x => panic!("std.spawn_thread called with {:?}", x),
                                }
                            }
                            Bi::MakeChan => match args.get(0).unwrap() {
                                Value::Unit => {
                                    let (s, r) = crossbeam_channel::unbounded();
                                    Value::Record(
                                        [
                                            ("send", Value::ChanSend(s)),
                                            ("recv", Value::ChanRecv(r)),
                                        ]
                                        .into_iter()
                                        .collect(),
                                    )
                                }
                                x => panic!("std.make_chan called with {:?}", x),
                            },
                            Bi::ChanSend => {
                                let chans = args.pop().unwrap();
                                let value = args.pop().unwrap();
                                assert!(args.is_empty());
                                match chans {
                                    Value::ChanSend(s) => match s.send(value) {
                                        Ok(()) => Value::Boolean(true),
                                        Err(_) => Value::Boolean(false),
                                    },
                                    x => panic!("std.chan_send called with {:?} (2nd argument)", x),
                                }
                            }
                            Bi::ChanRecv => match args.get(0).unwrap() {
                                Value::ChanRecv(r) => match r.recv() {
                                    Ok(x) => Value::Tagged {
                                        key: "Some",
                                        value: Box::new(x),
                                    },
                                    Err(_) => Value::Tagged {
                                        key: "None",
                                        value: Box::new(Value::Unit),
                                    },
                                },
                                x => panic!("std.chan_recv called with {:?}", x),
                            },
                            Bi::StdioWrite => {
                                match args.get(0).unwrap() {
                                    Value::PureString(s) => print!("{}", s),
                                    x => panic!("std.stdio.write called with {:?}", x),
                                }
                                Value::Unit
                            }
                        }
                    }
                }
                Value::Lambda {
                    argname,
                    f,
                    stacksave,
                } => run_stacksave(
                    env,
                    f,
                    &VarStack {
                        parent: Some(stack),
                        name: argname,
                        value: v_arg,
                    },
                    stacksave.into_iter(),
                ),
                Value::Tagger { key } => Value::Tagged {
                    key,
                    value: Box::new(v_arg),
                },
                f => panic!("called non-callable {:?} with argument {:?}", f, v_arg),
            }
        }
        Ek::Dot { prim, key } => match run(env, prim, stack) {
            Value::Record(mut rcm) => rcm
                .remove(&*key.inner)
                .expect("unable to find key in record"),
            x => panic!("called .{} on non-record {:?}", key.inner, x),
        },
        Ek::Fix { arg, body } => run(
            env,
            body,
            &VarStack {
                parent: Some(stack),
                name: arg,
                value: Value::FixLambda {
                    argname: arg,
                    f: body,
                },
            },
        ),
        Ek::FormatString(fsts) => {
            let mut r = String::new();
            for i in fsts {
                use core::fmt::Write;
                match run(env, i, stack) {
                    Value::PureString(s) => r += &s,
                    Value::Integer(i) => write!(&mut r, "{}", i).unwrap(),
                    Value::Boolean(b) => write!(&mut r, "_{}", if b { '1' } else { '0' }).unwrap(),
                    Value::Unit => {}
                    x => panic!("invoked format' stringify on non-stringifyable {:?}", x),
                }
            }
            Value::PureString(r)
        }
        Ek::Record(rcde) => {
            let mut rcd = BTreeMap::new();
            for (k, v) in rcde {
                rcd.insert(&**k, run(env, v, stack));
            }
            Value::Record(rcd)
        }
        Ek::Update { orig, ovrd } => {
            let v_orig = run(env, orig, stack);
            match run(env, ovrd, stack) {
                Value::Record(mut rcd) => {
                    match v_orig {
                        Value::Record(rcd_pull) => {
                            for (k, v) in rcd_pull.into_iter() {
                                if let std::collections::btree_map::Entry::Vacant(vac) =
                                    rcd.entry(k)
                                {
                                    vac.insert(v);
                                }
                            }
                        }
                        _ => panic!("invoked record update (lhs) on non-record {:?}", v_orig),
                    }
                    Value::Record(rcd)
                }
                v => panic!("invoked record update (rhs) on non-record {:?}", v),
            }
        }
        Ek::Tagger { key } => Value::Tagger { key: &*key },
        Ek::Match { inp, cases } => {
            let v_inp = run(env, inp, stack);
            let mut res = None;
            for i in cases {
                let mut coll = Default::default();
                if let Some(()) = run_pat(&mut coll, &i.pat, &v_inp) {
                    res = Some(run_stacksave(
                        env,
                        &i.body,
                        stack,
                        coll.into_iter().map(|(key, value)| (key, value.clone())),
                    ));
                    break;
                }
            }
            res.expect("disformed match")
        }
        Ek::Identifier(id) => {
            let r = stack.find(id).unwrap().clone();
            if let Value::FixLambda { argname, f } = r {
                run(
                    env,
                    f,
                    &VarStack {
                        parent: Some(stack),
                        name: argname,
                        value: Value::FixLambda { argname, f },
                    },
                )
            } else {
                r
            }
        }
        Ek::Unit => Value::Unit,
        Ek::Boolean(b) => Value::Boolean(*b),
        Ek::Integer(i) => Value::Integer(*i),
        Ek::PureString(s) => Value::PureString(s.clone()),
    };
    tracing::debug!(
        "expr@{} : {} : res={:?}",
        expr.offset,
        expr.inner.typ(),
        res
    );
    res
}
