use namada_core::dec::Dec;
use namada_core::uint::Uint;

#[derive(Clone, Debug)]
pub struct PDController {
    total_native_amount: Uint,
    max_reward_rate: Dec,
    last_inflation_amount: Uint,
    p_gain_nom: Dec,
    d_gain_nom: Dec,
    epochs_per_year: u64,
    target_metric: Dec,
    last_metric: Dec,
}

impl PDController {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        total_native_amount: Uint,
        max_reward_rate: Dec,
        last_inflation_amount: Uint,
        p_gain_nom: Dec,
        d_gain_nom: Dec,
        epochs_per_year: u64,
        target_metric: Dec,
        last_metric: Dec,
    ) -> PDController {
        PDController {
            total_native_amount,
            max_reward_rate,
            last_inflation_amount,
            p_gain_nom,
            d_gain_nom,
            epochs_per_year,
            target_metric,
            last_metric,
        }
    }

    pub fn compute_inflation(
        &self,
        control_coeff: Dec,
        current_metric: Dec,
    ) -> Uint {
        let control = self.compute_control(control_coeff, current_metric);
        self.compute_inflation_aux(control)
    }

    pub fn get_total_native_dec(&self) -> Dec {
        Dec::try_from(self.total_native_amount)
            .expect("Should not fail to convert Uint to Dec")
    }

    pub fn get_epochs_per_year(&self) -> u64 {
        self.epochs_per_year
    }

    fn get_max_inflation(&self) -> Uint {
        let total_native = self.get_total_native_dec();
        let epochs_py: Dec = self.epochs_per_year.into();

        let max_inflation = total_native * self.max_reward_rate / epochs_py;
        max_inflation
            .to_uint()
            .expect("Should not fail to convert Dec to Uint")
    }

    // TODO: could possibly use I256 instead of Dec here (need to account for
    // negative vals)
    fn compute_inflation_aux(&self, control: Dec) -> Uint {
        let last_inflation_amount = Dec::try_from(self.last_inflation_amount)
            .expect("Should not fail to convert Uint to Dec");
        let new_inflation_amount = last_inflation_amount + control;
        let new_inflation_amount = if new_inflation_amount.is_negative() {
            Uint::zero()
        } else {
            new_inflation_amount
                .to_uint()
                .expect("Should not fail to convert Dec to Uint")
        };

        let max_inflation = self.get_max_inflation();
        std::cmp::min(new_inflation_amount, max_inflation)
    }

    // NOTE: This formula is the comactification of all the old intermediate
    // computations that were done in multiple steps (as in the specs)
    fn compute_control(&self, coeff: Dec, current_metric: Dec) -> Dec {
        let val = current_metric * (self.d_gain_nom - self.p_gain_nom)
            + (self.target_metric * self.p_gain_nom)
            - (self.last_metric * self.d_gain_nom);
        coeff * val
    }
}
