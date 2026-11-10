//! Models with two prices: past a prompt size (272k tokens for the GPT
//! models) a whole call is billed at higher long-context rates. Both prices
//! are billed correctly, so which one a conversation uses is the user's
//! call. gz asks once, before the message that would take a conversation
//! there, remembers the answer for every such model, and `/autocompact`
//! changes it.

use crate::assistant::compaction;
use crate::assistant::models::{LongContextPricing, Selection};
use crate::code_ui::items::Item;

use super::{Effect, Picker, PickerRow, UiState};

/// How conversations on a model with two prices are kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceTier {
    /// Auto-compact under the size where the long-context rates start.
    Standard,
    /// Use the model's whole window and pay the long-context rates past it.
    FullWindow,
}

impl PriceTier {
    /// The profile keeps the choice as `code_long_context`: whether
    /// conversations may grow into the long-context rates.
    pub fn from_saved(long_context: Option<bool>) -> Option<Self> {
        long_context.map(|allowed| match allowed {
            true => Self::FullWindow,
            false => Self::Standard,
        })
    }

    pub fn to_saved(self) -> bool {
        self == Self::FullWindow
    }
}

/// One of the two choices, for the model it is offered on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceOffer {
    pub tier: PriceTier,
    /// Where the conversation auto-compacts on the standard price.
    pub standard_at: u64,
    pub pricing: LongContextPricing,
}

impl PriceOffer {
    pub fn label(&self) -> String {
        match self.tier {
            PriceTier::Standard => format!(
                "Standard price · auto-compact at {}",
                compaction::label(Some(self.standard_at))
            ),
            PriceTier::FullWindow => {
                let rates = self
                    .pricing
                    .rates()
                    .map(|rates| format!(" ({rates})"))
                    .unwrap_or_default();
                format!(
                    "Full window · long-context price past {}{rates}",
                    compaction::label(Some(self.pricing.threshold))
                )
            }
        }
    }
}

impl UiState {
    /// The model a turn runs on: `model`, else the server's default.
    fn selection_or_default(&self, model: Option<&Selection>) -> Option<Selection> {
        model
            .cloned()
            .or_else(|| self.catalog.as_ref()?.default_selection())
    }

    /// The two choices for the current model; `None` when it has one price
    /// or no auto-compact step fits under its long-context threshold.
    pub fn price_offers(&self) -> Option<[PriceOffer; 2]> {
        let selection = self.selection_or_default(self.model.as_ref())?;
        let pricing = selection.model.long_context()?.clone();
        let standard_at = compaction::standard_price_step(pricing.threshold)?;
        Some(
            [PriceTier::Standard, PriceTier::FullWindow].map(|tier| PriceOffer {
                tier,
                standard_at,
                pricing: pricing.clone(),
            }),
        )
    }

    /// The threshold that keeps `model` (the server's default when `None`)
    /// on its standard price, when the user chose that and it has two.
    pub(super) fn standard_price_threshold(&self, model: Option<&Selection>) -> Option<u64> {
        if self.price_tier != Some(PriceTier::Standard) {
            return None;
        }
        let selection = self.selection_or_default(model)?;
        compaction::standard_price_step(selection.model.long_context()?.threshold)
    }

    /// Where the next turn auto-compacts: the user's own threshold, else
    /// the standard-price one when that is their choice for this model.
    pub fn run_auto_compact(&self) -> Option<u64> {
        self.auto_compact
            .or_else(|| self.standard_price_threshold(self.model.as_ref()))
    }

    /// The choices to put to the user before a message goes out: only when
    /// they never chose, and the conversation has grown to where staying on
    /// the standard price means compacting.
    fn price_question(&self) -> Option<[PriceOffer; 2]> {
        if self.auto_compact.is_some() || self.price_tier.is_some() {
            return None;
        }
        let offers = self.price_offers()?;
        let tokens = self.usage.context.as_ref()?.tokens?;
        (tokens >= offers[0].standard_at).then_some(offers)
    }

    /// Holds a message for the price question; hands it back when there is
    /// nothing to ask.
    pub(super) fn hold_for_price(&mut self, effect: Effect) -> Option<Effect> {
        if !matches!(effect, Effect::Send(_)) {
            return Some(effect);
        }
        let Some(offers) = self.price_question() else {
            return Some(effect);
        };
        let model = self
            .selection_or_default(self.model.as_ref())
            .map_or_else(|| "This model".to_string(), |s| s.model.label());
        self.price_held = Some(effect);
        self.picker = Some(Picker {
            title: format!(
                "{model} costs more past {} tokens · your message waits",
                compaction::label(Some(offers[0].pricing.threshold))
            ),
            rows: offers.map(PickerRow::Price).to_vec(),
            selected: 0,
            model: None,
        });
        None
    }

    /// Whether the open picker is the question a message waits on.
    pub(super) fn is_price_question(&self) -> bool {
        self.price_held.is_some()
    }

    /// Carries out a price choice, from the question or from `/autocompact`,
    /// and sends the message that waited on it.
    pub(super) fn choose_price(&mut self, offer: &PriceOffer) -> Vec<Effect> {
        self.picker = None;
        self.price_tier = Some(offer.tier);
        let mut effects = vec![Effect::SetPriceTier(offer.tier)];
        // A price choice replaces a threshold picked by hand.
        if self.auto_compact.take().is_some() {
            effects.push(Effect::SetAutoCompact(None));
        }
        let choice = match offer.tier {
            PriceTier::Standard => format!(
                "Standard price: auto-compact at {} tokens",
                compaction::label(Some(offer.standard_at))
            ),
            PriceTier::FullWindow => format!(
                "Full window: long-context price past {} tokens",
                compaction::label(Some(offer.pricing.threshold))
            ),
        };
        self.record(Item::Notice(format!("{choice} · /autocompact changes it")));
        effects.extend(self.price_held.take());
        effects
    }

    /// Esc at the question: nothing is decided and nothing is sent.
    pub(super) fn dismiss_price_question(&mut self) {
        self.picker = None;
        if let Some(Effect::Send(text)) = self.price_held.take() {
            self.give_back(&text);
        }
        self.record(Item::Notice(
            "Not sent: pick a price to continue, or set one with /autocompact".into(),
        ));
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::*;
    use crate::assistant::models::GroundzeroMode;
    use crate::cmd::stream::ContextUsage;
    use crate::code_ui::state::test_support::*;

    /// A catalog whose `sol61` costs twice the input price past 272k.
    fn on_a_two_price_model(context_tokens: u64) -> UiState {
        let mut s = with_catalog();
        let catalog = s.catalog.as_mut().unwrap();
        catalog.modes.push(GroundzeroMode {
            value: "sol61".into(),
            label: "GPT-6.1 Sol".into(),
            efforts: vec![],
            context_window: Some(868_928),
            long_context: Some(LongContextPricing {
                threshold: 272_000,
                input_percent: Some(200),
                output_percent: Some(150),
            }),
        });
        s.model = catalog.resolve("sol61", None);
        s.usage.context = Some(ContextUsage {
            tokens: Some(context_tokens),
            window: 868_928,
            auto_compact_at: 738_588,
            history: None,
        });
        s
    }

    fn send(text: &str) -> Effect {
        Effect::Send(text.into())
    }

    #[test]
    fn a_conversation_nearing_the_higher_price_asks_once_before_sending() {
        let mut s = on_a_two_price_model(150_000);
        assert_eq!(s.gate(send("early")), vec![send("early")], "far from it");

        let mut s = on_a_two_price_model(214_000);
        assert!(s.gate(send("next")).is_empty(), "held for the answer");
        let picker = s.picker.as_ref().unwrap();
        assert_eq!(
            picker.title,
            "GPT-6.1 Sol costs more past 272k tokens · your message waits"
        );
        let PickerRow::Price(standard) = &picker.rows[0] else {
            panic!("{:?}", picker.rows);
        };
        let PickerRow::Price(full) = &picker.rows[1] else {
            panic!("{:?}", picker.rows);
        };
        assert_eq!(standard.label(), "Standard price · auto-compact at 200k");
        assert_eq!(
            full.label(),
            "Full window · long-context price past 272k (2× input, 1.5× output)"
        );

        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::SetPriceTier(PriceTier::Standard), send("next")]
        );
        assert_eq!(s.run_auto_compact(), Some(200_000));
        assert_eq!(s.gate(send("later")), vec![send("later")], "asked once");
    }

    #[test]
    fn choosing_the_full_window_sends_no_threshold_and_is_not_asked_again() {
        let mut s = on_a_two_price_model(214_000);
        s.gate(send("next"));
        s.on_key(key(KeyCode::Down));
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::SetPriceTier(PriceTier::FullWindow), send("next")]
        );
        assert_eq!(s.run_auto_compact(), None, "the server default: the window");
        assert_eq!(s.gate(send("later")), vec![send("later")]);
    }

    #[test]
    fn esc_decides_nothing_and_gives_the_message_back() {
        let mut s = on_a_two_price_model(214_000);
        s.gate(send("next"));
        assert!(s.on_key(key(KeyCode::Esc)).is_empty());
        assert_eq!(s.input.lines().join("\n"), "next");
        assert_eq!(s.price_tier, None);
        assert!(s.gate(send("next")).is_empty(), "asked again");
    }

    #[test]
    fn the_standard_price_choice_leaves_one_price_models_alone() {
        let mut s = on_a_two_price_model(214_000);
        s.price_tier = Some(PriceTier::Standard);
        assert_eq!(s.run_auto_compact(), Some(200_000));
        s.model = s.catalog.as_ref().unwrap().resolve("opus", None);
        assert_eq!(s.run_auto_compact(), None, "Claude Opus has one price");
        s.auto_compact = Some(500_000);
        s.model = s.catalog.as_ref().unwrap().resolve("sol61", None);
        assert_eq!(
            s.run_auto_compact(),
            Some(500_000),
            "a threshold picked by hand wins"
        );
    }

    #[test]
    fn a_threshold_picked_by_hand_is_never_questioned() {
        let mut s = on_a_two_price_model(214_000);
        s.auto_compact = Some(500_000);
        assert_eq!(s.gate(send("next")), vec![send("next")]);
        let mut s = on_a_two_price_model(214_000);
        s.model = s.catalog.as_ref().unwrap().resolve("opus", None);
        assert_eq!(s.gate(send("next")), vec![send("next")], "one price");
    }
}
