imports!();

use crate::fund_module::*;
use crate::types::fund_type::*;


/// Deals with storage data about delegators.
#[elrond_wasm_derive::module(FundTransformationsModuleImpl)]
pub trait FundTransformationsModule {

    #[module(FundModuleImpl)]
    fn fund_module(&self) -> FundModuleImpl<T, BigInt, BigUint>;

    #[module(FundTransformationsModuleImpl)]
    fn fund_transf_module(&self) -> FundTransformationsModuleImpl<T, BigInt, BigUint>;

    fn create_waiting(&self, user_id: usize, balance: BigUint) {
        self.fund_module().create_fund(user_id, FundDescription::Waiting, balance);
    }

    fn liquidate_free_stake(&self, user_id: usize, amount: &mut BigUint) -> SCResult<()> {
        // first withdraw from withdraw-only inactive stake
        sc_try!(self.fund_module().destroy_max_for_user(
            amount,
            user_id,
            FundType::WithdrawOnly));

        // if that is not enough, retrieve proper inactive stake
        if *amount > 0 {
            sc_try!(self.fund_module().destroy_max_for_user(
                amount,
                user_id,
                FundType::Waiting));
        }
        
        Ok(())
    }

    fn unstake_transf(&self, unstake_user_id: usize, amount: &BigUint) -> SCResult<()> {
        let mut amount_to_unstake = amount.clone();
        let current_bl_nonce = self.get_block_nonce();
        let _ = sc_try!(self.fund_module().split_convert_max_by_user(
            Some(&mut amount_to_unstake),
            unstake_user_id,
            FundType::Active,
            |_| Some(FundDescription::UnStaked{ created: current_bl_nonce })
        ));
        if amount_to_unstake > 0 {
            return sc_error!("cannot offer more than the user active stake");
        }

        Ok(())
    }

    fn swap_waiting_to_active(&self, amount: &BigUint) -> (Vec<usize>, BigUint) {
        let mut stake_to_activate = amount.clone();
        let mut affected_states = self.fund_module().split_convert_max_by_type(
            Some(&mut stake_to_activate),
            FundType::Waiting,
            |_, _| Some(FundDescription::Active)
        );
        affected_states.sort();
        affected_states.dedup();

        (affected_states, stake_to_activate)
    }

    fn swap_active_to_unstaked(&self, amount: &BigUint) -> (Vec<usize>, BigUint) {
        let mut amount_to_unstake = amount.clone();
        let current_bl_nonce = self.get_block_nonce();
        let mut affected_states = self.fund_module().split_convert_max_by_type(
            Some(&mut amount_to_unstake),
            FundType::Active,
            |_, _| Some(FundDescription::UnStaked{ created: current_bl_nonce })
        );
        affected_states.sort();
        affected_states.dedup();

        (affected_states, amount_to_unstake)
    }

    fn swap_unstaked_to_deferred_payment(&self, amount: &BigUint) -> BigUint {
        let mut unstaked_to_convert = amount.clone();
        let _ = self.fund_module().split_convert_max_by_type(
            Some(&mut unstaked_to_convert),
            FundType::UnStaked,
            |_, fund_info| match fund_info {
                FundDescription::UnStaked{ created } => Some(FundDescription::DeferredPayment{ created }),
               _ => None
            }
        );

        unstaked_to_convert
    }

    fn eligible_deferred_payment(&self, 
        user_id: usize, 
        n_blocks_before_claim: u64) -> BigUint {

        let current_bl_nonce = self.get_block_nonce();
        self.fund_module().query_sum_funds_by_user_type(
            user_id,
            FundType::DeferredPayment,
            |fund_desc| {
                if let FundDescription::DeferredPayment{ created } = fund_desc {
                    current_bl_nonce > created + n_blocks_before_claim 
                } else {
                    false
                }
            }
        )
    }

    fn claim_all_eligible_deferred_payments(&self,
        user_id: usize,
        n_blocks_before_claim: u64) -> SCResult<BigUint> {
        
        let current_bl_nonce = self.get_block_nonce();
        self.fund_module().split_convert_max_by_user(
            None,
            user_id,
            FundType::DeferredPayment,
            |fund_desc| {
                if let FundDescription::DeferredPayment{ created } = fund_desc {
                    if current_bl_nonce > created + n_blocks_before_claim {
                        return Some(FundDescription::WithdrawOnly)
                    }
                }
                None
            }
        )
    }
}
