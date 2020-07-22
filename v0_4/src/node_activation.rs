
use crate::auction_proxy::Auction;

use crate::bls_key::*;
use crate::node_state::*;
use crate::user_stake_state::*;

use crate::events::*;
use crate::node_config::*;
use crate::rewards::*;
use crate::settings::*;
use crate::user_data::*;
use crate::fund_transf_module::*;
use crate::fund_view_module::*;

imports!();

#[elrond_wasm_derive::module(NodeActivationModuleImpl)]
pub trait ContractStakeModule {

    #[module(UserDataModuleImpl)]
    fn user_data(&self) -> UserDataModuleImpl<T, BigInt, BigUint>;

    #[module(FundTransformationsModuleImpl)]
    fn fund_transf_module(&self) -> FundTransformationsModuleImpl<T, BigInt, BigUint>;

    #[module(FundViewModuleImpl)]
    fn fund_view_module(&self) -> FundViewModuleImpl<T, BigInt, BigUint>;

    #[module(SettingsModuleImpl)]
    fn settings(&self) -> SettingsModuleImpl<T, BigInt, BigUint>;

    #[module(EventsModuleImpl)]
    fn events(&self) -> EventsModuleImpl<T, BigInt, BigUint>;

    #[module(NodeConfigModuleImpl)]
    fn node_config(&self) -> NodeConfigModuleImpl<T, BigInt, BigUint>;

    #[module(RewardsModuleImpl)]
    fn rewards(&self) -> RewardsModuleImpl<T, BigInt, BigUint>;

    #[module(NodeActivationModuleImpl)]
    fn node_activation(&self) -> NodeActivationModuleImpl<T, BigInt, BigUint>;


    /// Owner activates specific nodes.
    #[endpoint(stakeNodes)]
    fn stake_nodes(&self,
            #[var_args] bls_keys: VarArgs<BLSKey>) -> SCResult<()> {

        if !self.settings().owner_called() {
            return sc_error!("only owner can activate nodes individually"); 
        }

        let mut node_ids = Vec::<usize>::with_capacity(bls_keys.len());
        let mut bls_keys_signatures = Vec::<Vec<u8>>::with_capacity(2 * bls_keys.len());
        for bls_key in bls_keys.iter() {
            let node_id = self.node_config().get_node_id(&bls_key);
            node_ids.push(node_id);
            bls_keys_signatures.push(bls_key.to_vec());
            bls_keys_signatures.push(self.node_config().get_node_signature(node_id).to_vec());
            if self.node_config().get_node_state(node_id) != NodeState::Inactive {
                return sc_error!("node not inactive");
            }
            self.node_config().set_node_state(node_id, NodeState::PendingActivation);
        }

        self.perform_stake_nodes(node_ids, bls_keys_signatures)
    }

    /// Stake as many nodes as necessary to activate the maximum possible stake.
    /// Anyone can call if auto activation is enabled.
    /// Error if auto activation is disabled (except owner, who can always call).
    #[endpoint(stakeAllAvailable)]
    fn stake_all_available_endpoint(&self) -> SCResult<()> {
        if !self.settings().caller_can_activate() {
            return sc_error!("not allowed to activate");
        }

        self.stake_all_available()
    }

    fn stake_all_available(&self) -> SCResult<()> {
        let mut inactive_stake = self.fund_view_module().get_user_stake_of_type(USER_STAKE_TOTALS_ID, UserStakeState::Inactive);
        let stake_per_node = self.node_config().get_stake_per_node();
        let num_nodes = self.node_config().get_num_nodes();
        let mut node_id = 1;
        let mut node_ids = Vec::<usize>::new();
        let mut bls_keys_signatures = Vec::<Vec<u8>>::new();
        while node_id <= num_nodes && inactive_stake >= stake_per_node {
            if self.node_config().get_node_state(node_id) == NodeState::Inactive {
                self.node_config().set_node_state(node_id, NodeState::PendingActivation);
                inactive_stake -= &stake_per_node;
                node_ids.push(node_id);
                bls_keys_signatures.push(self.node_config().get_node_id_to_bls(node_id).to_vec());
                bls_keys_signatures.push(self.node_config().get_node_signature(node_id).to_vec());
            }

            node_id += 1;
        }

        if node_ids.is_empty() {
            return Ok(())
        }

        self.perform_stake_nodes(node_ids, bls_keys_signatures)
    }

    fn perform_stake_nodes(&self, node_ids: Vec<usize>, bls_keys_signatures: Vec<Vec<u8>>) -> SCResult<()> {
        // do not launch nodes if owner hasn't staked enough
        sc_try!(self.fund_view_module().validate_owner_stake_share());

        let num_nodes = node_ids.len();

        let stake = BigUint::from(node_ids.len()) * self.node_config().get_stake_per_node();
        let mut stake_to_convert = stake.clone();
        self.fund_transf_module().activate_start_transf(&mut stake_to_convert);
        
        // send all stake to auction contract
        let auction_contract_addr = self.settings().get_auction_contract_address();
        let auction_contract = contract_proxy!(self, &auction_contract_addr, Auction);
        auction_contract.stake(
            node_ids, // callback arg
            num_nodes,
            bls_keys_signatures.into(),
            &stake);

        Ok(())
    }

    /// Only finalize activation if we got confirmation from the auction contract.
    /// #[callback] can only be declared in lib.rs for the moment.
    fn auction_stake_callback(&self, 
            node_ids: Vec<usize>, // #[callback_arg]
            call_result: AsyncCallResult<VarArgs<BLSStatusMultiArg>>) -> SCResult<()> {

        match call_result {
            AsyncCallResult::Ok(node_status_args) => {
                let (node_ids_ok, node_ids_fail) = self.node_config().split_node_ids_by_err(node_ids, node_status_args);
                sc_try!(self.auction_stake_callback_ok(node_ids_ok));
                sc_try!(self.auction_stake_callback_fail(node_ids_fail, &b"staking failed for some nodes"[..]));
                Ok(())
            },
            AsyncCallResult::Err(error) => {
                self.auction_stake_callback_fail(node_ids, error.err_msg.as_slice())
            }
        }
    }

    fn auction_stake_callback_ok(&self, node_ids: Vec<usize>) -> SCResult<()> {
        if node_ids.is_empty() {
            return Ok(());
        }

        // All rewards need to be recalculated now, 
        // because the rewardable stake changes.
        self.rewards().compute_all_rewards();

        // change user stake to Active
        let mut stake_activated = BigUint::from(node_ids.len()) * self.node_config().get_stake_per_node();
        self.fund_transf_module().activate_finish_ok_transf(&mut stake_activated);

        // set nodes to Active
        for &node_id in node_ids.iter() {
            self.node_config().set_node_state(node_id, NodeState::Active);
        }
        
        // log event (no data)
        // TODO: log BLS keys of nodes in data
        self.events().activation_ok_event(());

        Ok(())
    }

    fn auction_stake_callback_fail(&self, node_ids: Vec<usize>, err_msg: &[u8]) -> SCResult<()> {
        if node_ids.is_empty() {
            return Ok(());
        }

        // change user stake to ActivationFailed
        let mut stake_sent = BigUint::from(node_ids.len()) * self.node_config().get_stake_per_node();
        self.fund_transf_module().activate_finish_fail_transf(&mut stake_sent);

        // set nodes to ActivationFailed
        for &node_id in node_ids.iter() {
            self.node_config().set_node_state(node_id, NodeState::ActivationFailed);
        }

        // log failure event (no data)
        self.events().activation_fail_event(err_msg);

        Ok(())
    }

    // UNSTAKE

    /// Unstakes from the auction smart contract.
    /// The nodes will stop receiving rewards, but stake cannot be yet reclaimed.
    /// This operation is performed by the owner.
    #[endpoint(unStakeNodes)]
    fn unstake_nodes(&self,
            #[var_args] bls_keys: VarArgs<BLSKey>) -> SCResult<()> {

        if !self.settings().owner_called() {
            return sc_error!("only owner can deactivate nodes individually"); 
        }

        let mut node_ids = Vec::<usize>::with_capacity(bls_keys.len());
        for bls_key in bls_keys.iter() {
            let node_id = self.node_config().get_node_id(&bls_key);
            node_ids.push(node_id);
        }

        self.perform_unstake_nodes(None, node_ids, bls_keys.into_vec())
    }

    fn perform_unstake_nodes(&self,
            opt_requester: Option<usize>,
            node_ids: Vec<usize>,
            bls_keys: Vec<BLSKey>) -> SCResult<()> {

        // All rewards need to be recalculated now, 
        // because the rewardable stake will change shortly.
        self.rewards().compute_all_rewards();

        // convert node state to PendingDeactivation
        for &node_id in node_ids.iter() {
            if self.node_config().get_node_state(node_id) != NodeState::Active {
                return sc_error!("node not active");
            }
            self.node_config().set_node_state(node_id, NodeState::PendingDeactivation);
        }

        // convert funds to PendingDeactivation
        let mut stake_to_deactivate = BigUint::from(bls_keys.len()) * self.node_config().get_stake_per_node();
        let n_blocks_before_force_unstake = self.settings().get_n_blocks_before_force_unstake();
        self.fund_transf_module().unstake_start_transf(opt_requester, n_blocks_before_force_unstake, &mut stake_to_deactivate);

        // send unstake command to Auction SC
        let auction_contract_addr = self.settings().get_auction_contract_address();
        let auction_contract = contract_proxy!(self, &auction_contract_addr, Auction);
        auction_contract.unStake(
            node_ids,
            bls_keys.into());

        Ok(())
    }

    /// Only finalize deactivation if we got confirmation from the auction contract.
    /// #[callback] can only be declared in lib.rs for the moment.
    fn auction_unstake_callback(&self,
            node_ids: Vec<usize>, // #[callback_arg]
            call_result: AsyncCallResult<VarArgs<BLSStatusMultiArg>>) -> SCResult<()> {

        match call_result {
            AsyncCallResult::Ok(node_status_args) => {
                let (node_ids_ok, node_ids_fail) = self.node_config().split_node_ids_by_err(node_ids, node_status_args);
                sc_try!(self.auction_unstake_callback_ok(node_ids_ok));
                sc_try!(self.auction_unstake_callback_fail(node_ids_fail, &b"unstaking failed for some nodes"[..]));
                Ok(())
            },
            AsyncCallResult::Err(error) => {
                self.auction_unstake_callback_fail(node_ids, error.err_msg.as_slice())
            }
        }
    }

    fn auction_unstake_callback_ok(&self,
            node_ids: Vec<usize>) -> SCResult<()> {

        if node_ids.is_empty() {
            return Ok(());
        }

        // change user stake to UnBondPeriod
        let mut stake_sent = BigUint::from(node_ids.len()) * self.node_config().get_stake_per_node();
        self.fund_transf_module().unstake_finish_ok_transf(&mut stake_sent);

        // set nodes to UnBondPeriod + save current block nonce
        let bl_nonce = self.get_block_nonce();
        for &node_id in node_ids.iter() {
            self.node_config().set_node_state(node_id, NodeState::UnBondPeriod);
            self.node_config().set_node_bl_nonce_of_unstake(node_id, bl_nonce);
        }

        // log event (no data)
        // TODO: log BLS keys of nodes in data
        self.events().deactivation_ok_event(());

        Ok(())
    }

    fn auction_unstake_callback_fail(&self, node_ids: Vec<usize>, err_msg: &[u8]) -> SCResult<()> {
        if node_ids.is_empty() {
            return Ok(());
        }

        // Rewards must be clean because we are changing the active stake.
        // They were computed before calling auction unStake,
        // but in the unlikely case that rewards came in since then (between the asyncCall and the callback),
        // we recompute the rewards again.
        // Normally, all rewards should already be up to date, so this should add little to the gas cost.
        self.rewards().compute_all_rewards();

        // revert user stake to Active/ActiveForSale
        let mut stake_sent = BigUint::from(node_ids.len()) * self.node_config().get_stake_per_node();
        self.fund_transf_module().unstake_finish_fail_transf(&mut stake_sent);

        // revert nodes to Active
        for &node_id in node_ids.iter() {
            self.node_config().set_node_state(node_id, NodeState::Active);
        }

        // log failure event (no data)
        self.events().deactivation_fail_event(err_msg);

        Ok(())
    }

    // UNBOND

    /// Claims unstaked stake from the auction smart contract.
    /// This operation can be executed by anyone (note that it might cost much gas).
    #[endpoint(unBondNodes)]
    fn unbond_nodes(&self,
            #[var_args] bls_keys: VarArgs<BLSKey>) -> SCResult<()> {

        let bl_nonce = self.get_block_nonce();
        let n_blocks_before_unbond = self.settings().get_n_blocks_before_unbond();

        let mut node_ids = Vec::<usize>::with_capacity(bls_keys.len());
        for bls_key in bls_keys.iter() {
            let node_id = self.node_config().get_node_id(&bls_key);

            // check state
            if self.node_config().get_node_state(node_id) != NodeState::UnBondPeriod {
                return sc_error!("node not in unbond period");
            }

            // check that enough blocks passed
            let block_nonce_of_unstake = self.node_config().get_node_bl_nonce_of_unstake(node_id);
            if bl_nonce <= block_nonce_of_unstake + n_blocks_before_unbond {
                return sc_error!("too soon to unbond node");
            }

            node_ids.push(node_id);
            self.node_config().set_node_state(node_id, NodeState::PendingUnBond);
        }

        self.perform_unbond(node_ids, bls_keys.into_vec())
    }

    fn perform_unbond(&self,
        node_ids: Vec<usize>,
        bls_keys: Vec<BLSKey>) -> SCResult<()> {

        // change user stake to PendingUnBond
        let n_blocks_before_unbond = self.settings().get_n_blocks_before_unbond();
        let mut stake_to_unbond = BigUint::from(node_ids.len()) * self.node_config().get_stake_per_node();
        self.fund_transf_module().unbond_start_transf(n_blocks_before_unbond, &mut stake_to_unbond);

        
        if stake_to_unbond > 0 {
            return sc_error!("not enough stake in unbond period");
        }
        
        // send unbond command to Auction SC
        let auction_contract_addr = self.settings().get_auction_contract_address();
        let auction_contract = contract_proxy!(self, &auction_contract_addr, Auction);
        auction_contract.unBond(
            node_ids,
            bls_keys.into());

        Ok(())
    }

    /// Calls unbond for all nodes that are in the unbond period and are due.
    /// Anyone can call if they are willing to pay the gas.
    #[endpoint(unBondAllAvailable)]
    fn unbond_all_available(&self) -> SCResult<()> {
        let mut node_id = self.node_config().get_num_nodes();
        let mut node_ids = Vec::<usize>::new();
        let mut bls_keys = Vec::<BLSKey>::new();
        let bl_nonce = self.get_block_nonce();
        let n_blocks_before_unbond = self.settings().get_n_blocks_before_unbond();
        while node_id >= 1 {
            if self.node_config().get_node_state(node_id) == NodeState::UnBondPeriod {
                let bl_nonce_of_unstake = self.node_config().get_node_bl_nonce_of_unstake(node_id);
                if bl_nonce >= bl_nonce_of_unstake + n_blocks_before_unbond {
                    self.node_config().set_node_state(node_id, NodeState::PendingUnBond);
                    node_ids.push(node_id);
                    bls_keys.push(self.node_config().get_node_id_to_bls(node_id));
                }
            }

            node_id -= 1;
        }

        if node_ids.is_empty() {
            return Ok(())
        }

        self.perform_unbond(node_ids, bls_keys)
    }

    /// Only finalize deactivation if we got confirmation from the auction contract.
    /// #[callback] can only be declared in lib.rs for the moment.
    fn auction_unbond_callback(&self, 
        node_ids: Vec<usize>, // #[callback_arg]
        call_result: AsyncCallResult<VarArgs<BLSStatusMultiArg>>) -> SCResult<()> {

        match call_result {
            AsyncCallResult::Ok(node_status_args) => {
                let (node_ids_ok, node_ids_fail) = self.node_config().split_node_ids_by_err(node_ids, node_status_args);
                sc_try!(self.auction_unbond_callback_ok(node_ids_ok));
                sc_try!(self.auction_unbond_callback_fail(node_ids_fail, &b"unbonding failed for some nodes"[..]));
                Ok(())
            },
            AsyncCallResult::Err(error) => {
                self.auction_unbond_callback_fail(node_ids, error.err_msg.as_slice())
            }
        }
    }

    fn auction_unbond_callback_ok(&self, node_ids: Vec<usize>) -> SCResult<()> {
        if node_ids.is_empty() {
            return Ok(());
        }

        // set nodes to Inactive + reset unstake nonce since it is no longer needed
        for &node_id in node_ids.iter() {
            self.node_config().set_node_state(node_id, NodeState::Inactive);
            self.node_config().set_node_bl_nonce_of_unstake(node_id, 0);
        }

        // change user stake to Free
        let mut stake_to_unbond = BigUint::from(node_ids.len()) * self.node_config().get_stake_per_node();
        self.fund_transf_module().unbond_finish_ok_transf(&mut stake_to_unbond);
        if stake_to_unbond > 0 {
            return sc_error!("not enough stake pending unbond");
        }

        // log event (no data)
        // TODO: log BLS keys of nodes in data
        self.events().unbond_ok_event(());

        Ok(())
    }

    fn auction_unbond_callback_fail(&self, node_ids: Vec<usize>, err_msg: &[u8]) -> SCResult<()> {
        if node_ids.is_empty() {
            return Ok(());
        }
        
        // revert user stake to UnBondPeriod
        let mut stake_that_failed_unbond = BigUint::from(node_ids.len()) * self.node_config().get_stake_per_node();
        self.fund_transf_module().unbond_finish_fail_transf(&mut stake_that_failed_unbond);

        // revert nodes to UnBondPeriod
        for &node_id in node_ids.iter() {
            self.node_config().set_node_state(node_id, NodeState::UnBondPeriod);
        }

        // log failure event (no data)
        self.events().unbond_fail_event(err_msg);

        Ok(())
    }

    // CLAIM FAILED STAKE

    /// Claims unstaked stake from the auction smart contract.
    /// This operation can be executed by anyone (note that it might cost much gas).
    #[endpoint(claimFailedStake)]
    fn claim_failed_stake(&self) -> SCResult<()> {
        if !self.settings().owner_called() {
            return sc_error!("only owner can activate nodes individually"); 
        }

        let mut node_id = self.node_config().get_num_nodes();
        let mut node_ids = Vec::<usize>::new();
        while node_id >= 1 {
            if self.node_config().get_node_state(node_id) == NodeState::ActivationFailed {
                node_ids.push(node_id);
            }
            node_id -= 1;
        }

        if node_ids.is_empty() {
            return Ok(())
        }

        // send claim command to Auction SC
        let auction_contract_addr = self.settings().get_auction_contract_address();
        let auction_contract = contract_proxy!(self, &auction_contract_addr, Auction);
        auction_contract.claim(node_ids);

        Ok(())
    }

    /// Set nodes and stake to inactive, but only after call to auction claim completed.
    /// #[callback] can only be declared in lib.rs for the moment.
    fn auction_claim_callback(&self,
            node_ids: Vec<usize>, // #[callback_arg]
            call_result: AsyncCallResult<()>) -> SCResult<()> {

        match call_result {
            AsyncCallResult::Ok(()) => {
                // set nodes to Inactive
                for &node_id in node_ids.iter() {
                    self.node_config().set_node_state(node_id, NodeState::Inactive);
                }

                // revert user stake to Free
                let mut failed_stake = BigUint::from(node_ids.len()) * self.node_config().get_stake_per_node();
                self.fund_transf_module().claim_activation_failed_transf(&mut failed_stake);
            },
            AsyncCallResult::Err(_) => {
            }
        }

        Ok(())
    }

}