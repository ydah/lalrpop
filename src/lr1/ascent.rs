//! A compiler from an LR(1) table to a [recursive ascent] parser.
//!
//! [recursive ascent]: https://en.wikipedia.org/wiki/Recursive_ascent_parser

use intern::{intern, InternedString};
use grammar::repr::{Grammar, NonterminalString, Production, Symbol};
use lr1::{Action, Lookahead, State, StateIndex};
use rust::RustWrite;
use std::io::{self, Write};
use util::{Sep, Set, WorkSet};

pub type Path = Vec<InternedString>;

pub fn compile<'grammar>(grammar: &'grammar Grammar,
                         action_path: &Path,
                         start_symbol: NonterminalString,
                         states: &[State<'grammar>],
                         out: &mut RustWrite<&mut Write>)
                         -> io::Result<()>
{
    let mut ascent = RecursiveAscent::new(grammar, action_path, start_symbol, states, out);
    ascent.write()
}

struct RecursiveAscent<'ascent,'writer:'ascent,'grammar:'ascent> {
    grammar: &'grammar Grammar,
    action_path: &'ascent Path,
    start_symbol: NonterminalString,
    states: &'ascent [State<'grammar>],
    state_prefixes: Vec<&'grammar [Symbol]>,
    out: &'ascent mut RustWrite<&'writer mut Write>,
}

impl<'ascent,'writer,'grammar> RecursiveAscent<'ascent,'writer,'grammar> {
    fn new(grammar: &'grammar Grammar,
           action_path: &'ascent Path,
           start_symbol: NonterminalString,
           states: &'ascent [State<'grammar>],
           out: &'ascent mut RustWrite<&'writer mut Write>)
           -> RecursiveAscent<'ascent,'writer,'grammar>
    {
        let num_states = states.len();

        RecursiveAscent {
            grammar: grammar,
            states: states,
            state_prefixes: states.iter().map(|s| s.prefix()).collect(),
            action_path: action_path,
            start_symbol: start_symbol,
            out: out,
        }
    }

    fn write(&mut self) -> io::Result<()> {
        try!(self.write_terminal_use());
        rust!(self.out, "");

        try!(self.write_return_type_defn());
        rust!(self.out, "");

        try!(self.write_start_fn());
        rust!(self.out, "");

        for i in 0..self.states.len() {
            try!(self.write_state_fn(StateIndex(i)));
            rust!(self.out, "");
        }

        Ok(())
    }

    fn write_terminal_use(&mut self) -> io::Result<()> {
        rust!(self.out, "use {} as Terminal;",
              self.grammar.types.terminal_type());
        Ok(())
    }

    fn write_return_type_defn(&mut self) -> io::Result<()> {
        rust!(self.out, "enum Nonterminal {{");

        // make an enum with one variant per nonterminal; I considered
        // making different enums per state, but this would mean we
        // have to unwrap and rewrap as we pass up the stack, which
        // seems silly
        for &nt in self.grammar.productions.keys() {
            rust!(self.out, "{}({}),", nt, self.grammar.types.nonterminal_type(nt));
        }

        rust!(self.out, "}}");
        Ok(())
    }

    fn write_start_fn(&mut self) -> io::Result<()> {
        rust!(self.out, "pub fn start<TOKENS: Iterator<Item=Terminal>>(");
        rust!(self.out, "tokens: &mut TOKENS)");
        rust!(self.out, "-> Result<(Option<Terminal>, {}), Option<Terminal>>",
              self.grammar.types.nonterminal_type(self.start_symbol));
        rust!(self.out, "{{");

        rust!(self.out, "let mut lookahead = tokens.next();");
        rust!(self.out, "match try!(state0(lookahead, tokens)) {{");
        rust!(self.out, "(lookahead, Nonterminal::{}(nt)) => (lookahead, nt),",
              self.start_symbol);
        rust!(self.out, "_ => unreachable!(),");
        rust!(self.out, "}}");
        rust!(self.out, "}}");

        Ok(())
    }

    fn write_state_fn(&mut self, this_index: StateIndex) -> io::Result<()> {
        let this_state = &self.states[this_index.0];
        let this_prefix = self.state_prefixes[this_index.0];

        // Leave a comment explaining what this state is.
        rust!(self.out, "// State {}", this_index.0);
        for item in this_state.items.iter() {
            rust!(self.out, "//   {:?}", item);
        }
        rust!(self.out, "//");
        for (token, action) in &this_state.tokens {
            rust!(self.out, "//   {:?} -> {:?}", token, action);
        }
        rust!(self.out, "//");
        for (nt, state) in &this_state.gotos {
            rust!(self.out, "//   {:?} -> {:?}", nt, state);
        }

        // set to true if goto actions are worth generating
        let mut fallthrough = false;

        rust!(self.out, "fn state{}<TOKENS: Iterator<Item=Terminal>>(", this_index.0);
        rust!(self.out, "mut lookahead: Option<Terminal>,");
        rust!(self.out, "tokens: &mut TOKENS,");
        for i in 0..this_prefix.len() {
            rust!(self.out, "sym{}: &mut Option<{}>,",
                  i, this_prefix[i].ty(&self.grammar.types));
        }
        rust!(self.out, ") -> Result<(Option<Terminal>, Nonterminal), Option<Terminal>> {{");

        rust!(self.out, "let mut result;");

        rust!(self.out, "match lookahead {{");
        for (token, action) in &this_state.tokens {
            match *token {
                Lookahead::Terminal(s) =>
                    rust!(self.out, "Some({}) => {{", self.grammar.pattern(s)),
                Lookahead::EOF =>
                    rust!(self.out, "None => {{"),
            }

            match *action {
                Action::Shift(next_index) => {
                    // "shift" the lookahead onto the "stack" by taking its address
                    rust!(self.out, "let sym{} = &mut lookahead;", this_prefix.len());
                    rust!(self.out, "let lookahead = tokens.next();");

                    // transition to the new state
                    try!(self.transition(this_prefix, next_index, "result", "lookahead", "tokens"));
                    fallthrough = true;
                }

                Action::Reduce(production) => {
                    let n = this_prefix.len(); // number we have
                    let m = production.symbols.len(); // number action code wants
                    assert!(n >= m);
                    let transfer_syms = self.pop_syms(n, m);

                    // "pop" the items off the stack
                    for sym in &transfer_syms {
                        rust!(self.out, "let {} = {}.take().unwrap();", sym, sym);
                    }

                    // invoke the action code
                    rust!(self.out, "let nt = action{}({});",
                          production.action_fn.index(),
                          Sep(", ", &transfer_syms));

                    // wrap up the result along with the (unused) lookahead
                    if !transfer_syms.is_empty() {
                        // if we popped anything off of the stack, then this frame is done
                        rust!(self.out, "return Ok((lookahead, Nonterminal::{}(nt)));",
                              production.nonterminal);
                    } else {
                        // otherwise, pop back
                        rust!(self.out, "result = (lookahead, Nonterminal::{}(nt));",
                              production.nonterminal);
                        fallthrough = true;
                    }
                }
            }

            rust!(self.out, "}}");
        }

        // if we hit this, the next token is not recognized, so generate an error
        rust!(self.out, "_ => {{");
        rust!(self.out, "return Err(lookahead);");
        rust!(self.out, "}}");

        rust!(self.out, "}}"); // match

        if fallthrough && !this_state.gotos.is_empty() {
            // Handle goto table
            if this_prefix.len() > 0 {
                rust!(self.out, "while sym{}.is_some() {{", this_prefix.len() - 1);
            } else {
                rust!(self.out, "loop {{");
            }

            rust!(self.out, "let (lookahead, nt) = result;");

            rust!(self.out, "match nt {{");
            for (&nt, &next_index) in &this_state.gotos {
                rust!(self.out, "Nonterminal::{}(nt) => {{", nt);
                rust!(self.out, "let sym{} = &mut Some(nt);", this_prefix.len());
                try!(self.transition(this_prefix, next_index, "result", "lookahead", "tokens"));
                rust!(self.out, "}}");
            }

            // errors are not possible in the goto phase; a missing entry
            // indicates parse successfully completed, so just bail out
            rust!(self.out, "_ => {{");
            rust!(self.out, "return Ok((lookahead, nt));");
            rust!(self.out, "}}");

            rust!(self.out, "}}"); // match

            rust!(self.out, "}}"); // while/loop

            if this_prefix.len() > 0 {
                rust!(self.out, "return Ok(result);");
            }
        } else if fallthrough {
            rust!(self.out, "return Ok(result);");
        }

        rust!(self.out, "}}"); // fn

        Ok(())
    }

    fn pop_syms(&self, depth: usize, to_pop: usize) -> Vec<String> {
        (depth-to_pop .. depth).map(|i| format!("sym{}", i)).collect()
    }

    fn transition(&mut self,
                  prefix: &[Symbol],
                  next_index: StateIndex,
                  result: &str,
                  lookahead: &str,
                  tokens: &str)
                  -> io::Result<()>
    {
        // depth of stack, including the newly shifted token
        let n = prefix.len() + 1;

        // number of tokens next state expects; will
        // always be at least 1 for the newly shifted
        // token
        let m = self.state_prefixes[next_index.0].len();
        assert!(m >= 1);

        let transfer_syms = self.pop_syms(n, m);

        // invoke next state, transferring the top `m` tokens
        Ok(rust!(self.out, "{} = try!(state{}(lookahead, tokens, {}));",
                 result, next_index.0, Sep(", ", &transfer_syms)))
    }
}

