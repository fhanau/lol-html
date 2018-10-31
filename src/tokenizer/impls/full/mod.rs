#[macro_use]
mod actions;
mod conditions;

use base::{Align, Chunk, Range};
use errors::Error;
use std::cell::RefCell;
use std::rc::Rc;
pub use tokenizer::lex_unit::LexUnit;
pub use tokenizer::tag_name::TagName;
pub use tokenizer::token::*;
use tokenizer::tree_builder_simulator::*;

#[cfg(feature = "testing_api")]
pub use tokenizer::tree_builder_simulator::{TextParsingMode, TextParsingModeSnapshot};

const DEFAULT_ATTR_BUFFER_CAPACITY: usize = 256;

pub trait LexUnitHandler {
    fn handle(&mut self, lex_unit: &LexUnit);
}

#[cfg(feature = "testing_api")]
impl<F: FnMut(&LexUnit)> LexUnitHandler for F {
    fn handle(&mut self, lex_unit: &LexUnit) {
        self(lex_unit);
    }
}

pub enum ParsingLoopDirective {
    Break,
    Continue,
}

pub type TokenizerState<H> = fn(&mut Tokenizer<H>, &Chunk) -> Result<ParsingLoopDirective, Error>;

pub struct Tokenizer<H: LexUnitHandler> {
    next_pos: usize,
    lex_unit_start: usize,
    token_part_start: usize,
    state_enter: bool,
    allow_cdata: bool,
    lex_unit_handler: H,
    state: TokenizerState<H>,
    current_token: Option<TokenView>,
    current_attr: Option<AttributeView>,
    last_start_tag_name_hash: Option<u64>,
    closing_quote: u8,
    attr_buffer: Rc<RefCell<Vec<AttributeView>>>,
    tree_builder_simulator: TreeBuilderSimulator,

    #[cfg(feature = "testing_api")]
    text_parsing_mode_change_handler: Option<Box<dyn TextParsingModeChangeHandler>>,
}

impl<H: LexUnitHandler> Tokenizer<H> {
    pub fn new(lex_unit_handler: H) -> Self {
        Tokenizer {
            next_pos: 0,
            lex_unit_start: 0,
            token_part_start: 0,
            state_enter: true,
            allow_cdata: false,
            lex_unit_handler,
            state: Tokenizer::data_state,
            current_token: None,
            current_attr: None,
            last_start_tag_name_hash: None,
            closing_quote: b'"',
            attr_buffer: Rc::new(RefCell::new(Vec::with_capacity(
                DEFAULT_ATTR_BUFFER_CAPACITY,
            ))),
            tree_builder_simulator: TreeBuilderSimulator::default(),

            #[cfg(feature = "testing_api")]
            text_parsing_mode_change_handler: None,
        }
    }

    define_state_machine!();

    pub fn tokenize(&mut self, input: &Chunk) -> Result<usize, Error> {
        loop {
            let directive = (self.state)(self, input)?;

            if let ParsingLoopDirective::Break = directive {
                break;
            }
        }

        Ok(input.len() - self.lex_unit_start)
    }

    pub fn align_offsets_to_blocked_bytes_start(&mut self) {
        self.next_pos.align(self.lex_unit_start + 1);
        self.token_part_start.align(self.lex_unit_start);
        self.current_token.align(self.lex_unit_start);
        self.current_attr.align(self.lex_unit_start);

        self.lex_unit_start = 0;
    }

    #[cfg(feature = "testing_api")]
    pub fn set_state(&mut self, state: TokenizerState<H>) {
        self.state = state;
    }

    #[cfg(feature = "testing_api")]
    pub fn set_last_start_tag_name_hash(&mut self, name_hash: Option<u64>) {
        self.last_start_tag_name_hash = name_hash;
    }

    #[cfg(feature = "testing_api")]
    pub fn set_text_parsing_mode_change_handler(
        &mut self,
        handler: Box<dyn TextParsingModeChangeHandler>,
    ) {
        self.text_parsing_mode_change_handler = Some(handler);
    }

    fn handle_tree_builder_feedback(
        &mut self,
        feedback: TreeBuilderFeedback,
        lex_unit: &LexUnit,
    ) -> Option<ParsingLoopDirective> {
        let mut feedback = feedback;

        loop {
            match feedback {
                TreeBuilderFeedback::Adjust(adjustment) => {
                    return self.apply_adjustment(adjustment);
                }
                TreeBuilderFeedback::RequestStartTagToken(reason) => {
                    let token = lex_unit
                        .get_token()
                        .expect("There should be a token at this point");

                    feedback = self
                        .tree_builder_simulator
                        .fulfill_start_tag_token_request(&token, reason);
                }
                TreeBuilderFeedback::RequestEndTagToken => {
                    let token = lex_unit
                        .get_token()
                        .expect("There should be a token at this point");

                    feedback = self
                        .tree_builder_simulator
                        .fulfill_end_tag_token_request(&token);
                }
                TreeBuilderFeedback::RequestSelfClosingFlag => match lex_unit.get_token_view() {
                    Some(&TokenView::StartTag { self_closing, .. }) => {
                        feedback = self
                            .tree_builder_simulator
                            .fulfill_self_closing_flag_request(self_closing);
                    }
                    _ => unreachable!("Token should be a start tag at this point"),
                },
                TreeBuilderFeedback::None => break,
            }
        }

        None
    }

    fn apply_adjustment(
        &mut self,
        adjustment: TokenizerAdjustment,
    ) -> Option<ParsingLoopDirective> {
        match adjustment {
            TokenizerAdjustment::SwitchTextParsingMode(mode) => {
                notify_text_parsing_mode_change!(self, mode);
                self.switch_state(mode.into());
                Some(ParsingLoopDirective::Continue)
            }
            TokenizerAdjustment::SetAllowCdata(allow_cdata) => {
                self.allow_cdata = allow_cdata;
                None
            }
        }
    }

    #[inline]
    pub(super) fn switch_state(&mut self, state: TokenizerState<H>) {
        self.state = state;
        self.state_enter = true;
    }

    #[inline]
    pub(super) fn emit_lex_unit<'c>(
        &mut self,
        input: &'c Chunk,
        token: Option<TokenView>,
        raw_range: Option<Range>,
    ) -> LexUnit<'c> {
        let lex_unit = LexUnit::new(input, token, raw_range);

        self.lex_unit_handler.handle(&lex_unit);

        lex_unit
    }

    #[inline]
    fn emit_lex_unit_with_raw<'c>(
        &mut self,
        input: &'c Chunk,
        token: Option<TokenView>,
        raw_end: usize,
    ) -> LexUnit<'c> {
        let raw_range = Some(Range {
            start: self.lex_unit_start,
            end: raw_end,
        });

        self.lex_unit_start = raw_end;

        self.emit_lex_unit(input, token, raw_range)
    }

    #[inline]
    pub(super) fn emit_lex_unit_with_raw_inclusive<'c>(
        &mut self,
        input: &'c Chunk,
        token: Option<TokenView>,
    ) -> LexUnit<'c> {
        let raw_end = input!(@pos self) + 1;

        self.emit_lex_unit_with_raw(input, token, raw_end)
    }

    #[inline]
    pub(super) fn emit_lex_unit_with_raw_exclusive<'c>(
        &mut self,
        input: &'c Chunk,
        token: Option<TokenView>,
    ) -> LexUnit<'c> {
        let raw_end = input!(@pos self);

        self.emit_lex_unit_with_raw(input, token, raw_end)
    }
}
