use super::node_storage::types::*;
use crate::auction_proxy::*;

use super::node_storage::node_config::*;
use super::user_fund_storage::user_data::*;
use crate::events::*;
use crate::reset_checkpoints::*;
use crate::rewards::*;
use crate::settings::*;
use crate::user_stake::*;

elrond_wasm::imports!();

#[elrond_wasm_derive::module(NodeActivationModuleImpl)]
pub trait ContractStakeModule {
    #[module(UserDataModuleImpl)]
    fn user_data(&self) -> UserDataModuleImpl<T, BigInt, BigUint>;

    #[module(SettingsModuleImpl)]
    fn settings(&self) -> SettingsModuleImpl<T, BigInt, BigUint>;

    #[module(EventsModuleImpl)]
    fn events(&self) -> EventsModuleImpl<T, BigInt, BigUint>;

    #[module(NodeConfigModuleImpl)]
    fn node_config(&self) -> NodeConfigModuleImpl<T, BigInt, BigUint>;

    #[module(RewardsModuleImpl)]
    fn rewards(&self) -> RewardsModuleImpl<T, BigInt, BigUint>;

    #[module(UserStakeModuleImpl)]
    fn user_stake(&self) -> UserStakeModuleImpl<T, BigInt, BigUint>;

    #[module(ResetCheckpointsModuleImpl)]
    fn reset_checkpoints(&self) -> ResetCheckpointsModuleImpl<T, BigInt, BigUint>;

    /// Owner activates specific nodes.
    #[endpoint(stakeNodes)]
    fn stake_nodes(
        &self,
        amount_to_stake: BigUint,
        #[var_args] bls_keys: VarArgs<BLSKey>,
    ) -> SCResult<AsyncCall<BigUint>> {
        only_owner!(self, "only owner allowed to stake nodes");

        require!(
            !self.settings().is_bootstrap_mode(),
            "cannot stake nodes in bootstrap mode"
        );

        require!(
            !self.reset_checkpoints().is_global_op_in_progress(),
            "node operations are temporarily paused as checkpoint is reset"
        );

        require!(
            self.rewards().total_unprotected() >= amount_to_stake,
            "not enough funds in contract to stake nodes"
        );

        sc_try!(self.user_stake().validate_owner_stake_share());

        let mut node_ids = Vec::<usize>::with_capacity(bls_keys.len());
        let mut bls_keys_signatures: Vec<MultiArg2<BLSKey, BLSSignature>> = Vec::new();

        for bls_key in bls_keys.into_vec().into_iter() {
            let node_id = self.node_config().get_node_id(&bls_key);
            require!(node_id != 0, "unknown node provided");

            require!(
                self.node_config().get_node_state(node_id) == NodeState::Inactive,
                "node must be inactive"
            );

            node_ids.push(node_id);
            let bls_signature = self.node_config().get_node_signature(node_id);
            bls_keys_signatures.push((bls_key, bls_signature).into());

            self.node_config()
                .set_node_state(node_id, NodeState::PendingActivation);
        }

        Ok(self.perform_stake_nodes(node_ids, bls_keys_signatures.into(), amount_to_stake))
    }

    fn perform_stake_nodes(
        &self,
        node_ids: Vec<usize>,
        bls_keys_signatures: VarArgs<MultiArg2<BLSKey, BLSSignature>>,
        amount_to_stake: BigUint,
    ) -> AsyncCall<BigUint> {
        let num_nodes = node_ids.len();
        // send all stake to auction contract
        let auction_contract_addr = self.settings().get_auction_contract_address();

        contract_call!(self, auction_contract_addr, AuctionProxy)
            .with_token_transfer(TokenIdentifier::egld(), amount_to_stake)
            .stake(num_nodes, bls_keys_signatures)
            .async_call()
            .with_callback(self.callbacks().auction_stake_callback(node_ids))
    }

    /// Only finalize activation if we got confirmation from the auction contract.
    /// `#[callback]` also has be declared in lib.rs for the moment.
    #[callback]
    fn auction_stake_callback(
        &self,
        node_ids: Vec<usize>, // #[callback_arg]
        #[call_result] call_result: AsyncCallResult<MultiResultVec<BLSStatusMultiArg>>,
    ) -> SCResult<()> {
        match call_result {
            AsyncCallResult::Ok(node_status_args) => {
                let (node_ids_ok, node_ids_fail) = self
                    .node_config()
                    .split_node_ids_by_err(node_ids, node_status_args);
                sc_try!(self.auction_stake_callback_ok(node_ids_ok));
                sc_try!(self.auction_stake_callback_fail(
                    node_ids_fail,
                    &b"staking failed for some nodes"[..]
                ));
                Ok(())
            }
            AsyncCallResult::Err(error) => {
                self.auction_stake_callback_fail(node_ids, error.err_msg.as_slice())
            }
        }
    }

    fn auction_stake_callback_ok(&self, node_ids: Vec<usize>) -> SCResult<()> {
        if node_ids.is_empty() {
            return Ok(());
        }

        // set nodes to Active
        for &node_id in node_ids.iter() {
            self.node_config()
                .set_node_state(node_id, NodeState::Active);
        }

        // log event (no data)
        // TODO: log BLS keys of nodes in data
        self.events().stake_node_ok_event(());

        Ok(())
    }

    fn auction_stake_callback_fail(&self, node_ids: Vec<usize>, err_msg: &[u8]) -> SCResult<()> {
        if node_ids.is_empty() {
            return Ok(());
        }

        // set nodes to Inactive
        for &node_id in node_ids.iter() {
            self.node_config()
                .set_node_state(node_id, NodeState::Inactive);
        }

        // log failure event (no data)
        self.events().stake_node_fail_event(err_msg);

        Ok(())
    }

    // UNSTAKE

    /// Unstakes from the auction smart contract.
    /// The nodes will stop receiving rewards, but stake cannot be yet reclaimed.
    /// This operation is performed by the owner.
    /// Does not unstake tokens.
    #[endpoint(unStakeNodes)]
    fn unstake_nodes_endpoint(
        &self,
        #[var_args] bls_keys: VarArgs<BLSKey>,
    ) -> SCResult<AsyncCall<BigUint>> {
        self.unstake_nodes(false, bls_keys)
    }

    /// Unstakes from the auction smart contract.
    /// The nodes will stop receiving rewards, but stake cannot be yet reclaimed.
    /// This operation is performed by the owner.
    /// Also unstakes tokens.
    #[endpoint(unStakeNodesAndTokens)]
    fn unstake_nodes_and_tokens_endpoint(
        &self,
        #[var_args] bls_keys: VarArgs<BLSKey>,
    ) -> SCResult<AsyncCall<BigUint>> {
        self.unstake_nodes(true, bls_keys)
    }

    fn unstake_nodes(
        &self,
        unstake_tokens: bool,
        bls_keys: VarArgs<BLSKey>,
    ) -> SCResult<AsyncCall<BigUint>> {
        only_owner!(self, "only owner allowed to unstake nodes");

        require!(
            !self.reset_checkpoints().is_global_op_in_progress(),
            "node operations are temporarily paused as checkpoint is reset"
        );

        let mut node_ids = Vec::<usize>::with_capacity(bls_keys.len());
        for bls_key in bls_keys.iter() {
            let node_id = self.node_config().get_node_id(&bls_key);
            require!(node_id != 0, "unknown node provided");
            node_ids.push(node_id);
        }

        self.perform_unstake_nodes(unstake_tokens, node_ids, bls_keys.into_vec())
    }

    fn perform_unstake_nodes(
        &self,
        unstake_tokens: bool,
        node_ids: Vec<usize>,
        bls_keys: Vec<BLSKey>,
    ) -> SCResult<AsyncCall<BigUint>> {
        // convert node state to PendingDeactivation
        for &node_id in node_ids.iter() {
            require!(
                self.node_config().get_node_state(node_id) == NodeState::Active,
                "node not active"
            );

            self.node_config()
                .set_node_state(node_id, NodeState::PendingDeactivation);
        }

        // send unstake command to Auction SC
        let auction_contract_addr = self.settings().get_auction_contract_address();
        let auction_proxy = contract_call!(self, auction_contract_addr, AuctionProxy);
        if unstake_tokens {
            Ok(auction_proxy
                .unStake(bls_keys.into())
                .async_call()
                .with_callback(self.callbacks().auction_unstake_callback(node_ids)))
        } else {
            Ok(auction_proxy
                .unStakeNodes(bls_keys.into())
                .async_call()
                .with_callback(self.callbacks().auction_unstake_callback(node_ids)))
        }
    }

    /// Only finalize deactivation if we got confirmation from the auction contract.
    /// `#[callback]` also has be declared in lib.rs for the moment.
    #[callback]
    fn auction_unstake_callback(
        &self,
        node_ids: Vec<usize>, // #[callback_arg]
        #[call_result] call_result: AsyncCallResult<MultiResultVec<BLSStatusMultiArg>>,
    ) -> SCResult<()> {
        match call_result {
            AsyncCallResult::Ok(node_status_args) => {
                let (node_ids_ok, node_ids_fail) = self
                    .node_config()
                    .split_node_ids_by_err(node_ids, node_status_args);
                sc_try!(self.auction_unstake_callback_ok(node_ids_ok));
                sc_try!(self.auction_unstake_callback_fail(
                    node_ids_fail,
                    &b"unstaking failed for some nodes"[..]
                ));
                Ok(())
            }
            AsyncCallResult::Err(error) => {
                self.auction_unstake_callback_fail(node_ids, error.err_msg.as_slice())
            }
        }
    }

    fn auction_unstake_callback_ok(&self, node_ids: Vec<usize>) -> SCResult<()> {
        if node_ids.is_empty() {
            return Ok(());
        }

        // set nodes to UnBondPeriod + save current block nonce
        let bl_nonce = self.get_block_nonce();
        for &node_id in node_ids.iter() {
            self.node_config()
                .set_node_state(node_id, NodeState::UnBondPeriod { started: bl_nonce });
        }

        // log event (no data)
        // TODO: log BLS keys of nodes in data
        self.events().unstake_node_ok_event(());

        Ok(())
    }

    fn auction_unstake_callback_fail(&self, node_ids: Vec<usize>, err_msg: &[u8]) -> SCResult<()> {
        if node_ids.is_empty() {
            return Ok(());
        }

        // revert nodes to Active
        for &node_id in node_ids.iter() {
            self.node_config()
                .set_node_state(node_id, NodeState::Active);
        }

        // log failure event (no data)
        self.events().unstake_node_fail_event(err_msg);

        Ok(())
    }

    // UNBOND
    /// Calls unbond for all provided nodes. Will fail if node cannot be unbonded.
    #[endpoint(unBondNodes)]
    fn unbond_specific_nodes_endpoint(
        &self,
        #[var_args] bls_keys: VarArgs<BLSKey>,
    ) -> SCResult<AsyncCall<BigUint>> {
        self.unbond_specific_nodes(bls_keys)
    }

    fn unbond_specific_nodes(
        &self,
        bls_keys: VarArgs<BLSKey>,
    ) -> SCResult<AsyncCall<BigUint>> {
        only_owner!(self, "only owner allowed to unbond nodes");

        require!(
            !self.reset_checkpoints().is_global_op_in_progress(),
            "node operations are temporarily paused as checkpoint is reset"
        );

        require!(!bls_keys.is_empty(), "no BLS keys provided");

        let mut node_ids = Vec::<usize>::with_capacity(bls_keys.len());
        for bls_key in bls_keys.iter() {
            let node_id = self.node_config().get_node_id(&bls_key);
            require!(node_id != 0, "unknown node provided");
            require!(
                self.prepare_node_for_unbond_if_possible(node_id),
                "node cannot be unbonded"
            );
            node_ids.push(node_id);
        }

        Ok(self.perform_unbond(node_ids, bls_keys.into_vec()))
    }

    /// Calls unbond for all nodes that are in the unbond period and are due.
    /// Nothing happens if no nodes can be unbonded.
    #[endpoint(unBondAllPossibleNodes)]
    fn unbond_all_possible_nodes(&self) -> SCResult<OptionalResult<AsyncCall<BigUint>>> {
        only_owner!(self, "only owner allowed to unbond nodes");

        require!(
            !self.reset_checkpoints().is_global_op_in_progress(),
            "node operations are temporarily paused as checkpoint is reset"
        );

        let mut node_id = self.node_config().num_nodes().get();
        let mut node_ids = Vec::<usize>::new();
        let mut bls_keys = Vec::<BLSKey>::new();
        while node_id >= 1 {
            if self.prepare_node_for_unbond_if_possible(node_id) {
                node_ids.push(node_id);
                bls_keys.push(self.node_config().get_node_id_to_bls(node_id));
            }

            node_id -= 1;
        }

        if node_ids.is_empty() {
            return Ok(OptionalResult::None);
        }

        Ok(OptionalResult::Some(
            self.perform_unbond(node_ids, bls_keys),
        ))
    }

    fn prepare_node_for_unbond_if_possible(&self, node_id: usize) -> bool {
        if let NodeState::UnBondPeriod { started } = self.node_config().get_node_state(node_id) {
            self.node_config().set_node_state(
                node_id,
                NodeState::PendingUnBond {
                    unbond_started: started,
                },
            );
            return true;
        }

        false
    }

    fn perform_unbond(
        &self,
        node_ids: Vec<usize>,
        bls_keys: Vec<BLSKey>,
    ) -> AsyncCall<BigUint> {
        // send unbond command to Auction SC
        let auction_contract_addr = self.settings().get_auction_contract_address();
        contract_call!(self, auction_contract_addr, AuctionProxy)
            .unBondNodes(bls_keys.into())
            .async_call()
            .with_callback(self.callbacks().auction_unbond_callback(node_ids))
        
    }

    /// Only finalize deactivation if we got confirmation from the auction contract.
    /// `#[callback]` also has be declared in lib.rs for the moment.
    #[callback]
    fn auction_unbond_callback(
        &self,
        node_ids: Vec<usize>,
        #[call_result] call_result: AsyncCallResult<MultiResultVec<BLSStatusMultiArg>>,
    ) -> SCResult<()> {
        match call_result {
            AsyncCallResult::Ok(node_status_args) => {
                let (node_ids_ok, node_ids_fail) = self
                    .node_config()
                    .split_node_ids_by_err(node_ids, node_status_args);
                sc_try!(self.auction_unbond_callback_ok(node_ids_ok));
                sc_try!(self.auction_unbond_callback_fail(
                    node_ids_fail,
                    &b"unbonding failed for some nodes"[..]
                ));
                Ok(())
            }
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
            self.node_config()
                .set_node_state(node_id, NodeState::Inactive);
        }

        // log event (no data)
        // TODO: log BLS keys of nodes in data
        self.events().unbond_node_ok_event(());

        Ok(())
    }

    fn auction_unbond_callback_fail(&self, node_ids: Vec<usize>, err_msg: &[u8]) -> SCResult<()> {
        if node_ids.is_empty() {
            return Ok(());
        }

        // revert nodes to UnBondPeriod
        for &node_id in node_ids.iter() {
            if let NodeState::PendingUnBond { unbond_started } =
                self.node_config().get_node_state(node_id)
            {
                self.node_config().set_node_state(
                    node_id,
                    NodeState::UnBondPeriod {
                        started: unbond_started,
                    },
                );
            } else {
                return sc_error!("node not pending unbond");
            }
        }

        // log failure event (no data)
        self.events().unbond_node_fail_event(err_msg);

        Ok(())
    }

    /// Claims from auction SC funds that were sent but are not required to run the nodes.
    #[endpoint(claimUnusedFunds)]
    fn claim_unused_funds(&self) -> SCResult<AsyncCall<BigUint>> {
        only_owner!(self, "only owner can claim inactive stake from auction");

        require!(
            !self.reset_checkpoints().is_global_op_in_progress(),
            "node operations are temporarily paused as checkpoint is reset"
        );

        // send claim command to Auction SC
        let auction_contract_addr = self.settings().get_auction_contract_address();
        Ok(contract_call!(self, auction_contract_addr, AuctionProxy)
            .claim()
            .async_call())
    }

    #[payable("EGLD")]
    #[endpoint(unJailNodes)]
    fn unjail_nodes(
        &self,
        #[var_args] bls_keys: VarArgs<BLSKey>,
        #[payment] fine_payment: BigUint,
    ) -> SCResult<AsyncCall<BigUint>> {
        only_owner!(self, "only owner allowed to unjail nodes");

        // validation only
        for bls_key in bls_keys.iter() {
            let node_id = self.node_config().get_node_id(&bls_key);
            require!(node_id != 0, "unknown node provided");
            require!(
                self.node_config().get_node_state(node_id) == NodeState::Active,
                "node must be active"
            );
        }

        // send unJail command to Auction SC
        let auction_contract_addr = self.settings().get_auction_contract_address();
        Ok(contract_call!(self, auction_contract_addr, AuctionProxy)
            .with_token_transfer(TokenIdentifier::egld(), fine_payment)
            .unJail(bls_keys)
            .async_call())
    }
}
