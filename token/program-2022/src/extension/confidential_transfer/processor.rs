use {
    crate::{
        check_program_account, check_zk_token_proof_program_account,
        error::TokenError,
        extension::{
            confidential_transfer::{instruction::*, *},
            confidential_transfer_fee::{
                ConfidentialTransferFeeAmount, ConfidentialTransferFeeConfig, EncryptedFee,
                EncryptedWithheldAmount,
            },
            memo_transfer::{check_previous_sibling_instruction_is_memo, memo_required},
            transfer_fee::TransferFeeConfig,
            BaseStateWithExtensions, StateWithExtensions, StateWithExtensionsMut,
        },
        instruction::{decode_instruction_data, decode_instruction_type},
        processor::Processor,
        proof::decode_proof_instruction_context,
        solana_zk_token_sdk::zk_token_elgamal::pod::TransferAmountCiphertext,
        state::{Account, Mint},
    },
    solana_program::{
        account_info::{next_account_info, AccountInfo},
        clock::Clock,
        entrypoint::ProgramResult,
        msg,
        program_error::ProgramError,
        pubkey::Pubkey,
        sysvar::instructions::get_instruction_relative,
        sysvar::Sysvar,
    },
};
// Remove feature once zk ops syscalls are enabled on all networks
#[cfg(feature = "zk-ops")]
use {
    crate::extension::non_transferable::NonTransferable,
    solana_zk_token_sdk::zk_token_elgamal::ops as syscall,
};

/// Processes an [InitializeMint] instruction.
fn process_initialize_mint(
    accounts: &[AccountInfo],
    authority: &OptionalNonZeroPubkey,
    auto_approve_new_account: PodBool,
    auditor_encryption_pubkey: &OptionalNonZeroElGamalPubkey,
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let mint_info = next_account_info(account_info_iter)?;

    check_program_account(mint_info.owner)?;
    let mint_data = &mut mint_info.data.borrow_mut();
    let mut mint = StateWithExtensionsMut::<Mint>::unpack_uninitialized(mint_data)?;
    let confidential_transfer_mint = mint.init_extension::<ConfidentialTransferMint>(true)?;

    confidential_transfer_mint.authority = *authority;
    confidential_transfer_mint.auto_approve_new_accounts = auto_approve_new_account;
    confidential_transfer_mint.auditor_elgamal_pubkey = *auditor_encryption_pubkey;

    Ok(())
}

/// Processes an [UpdateMint] instruction.
fn process_update_mint(
    accounts: &[AccountInfo],
    auto_approve_new_account: PodBool,
    auditor_encryption_pubkey: &OptionalNonZeroElGamalPubkey,
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let mint_info = next_account_info(account_info_iter)?;
    let authority_info = next_account_info(account_info_iter)?;

    check_program_account(mint_info.owner)?;
    let mint_data = &mut mint_info.data.borrow_mut();
    let mut mint = StateWithExtensionsMut::<Mint>::unpack(mint_data)?;
    let confidential_transfer_mint = mint.get_extension_mut::<ConfidentialTransferMint>()?;
    let maybe_confidential_transfer_mint_authority: Option<Pubkey> =
        confidential_transfer_mint.authority.into();
    let confidential_transfer_mint_authority =
        maybe_confidential_transfer_mint_authority.ok_or(TokenError::NoAuthorityExists)?;

    if !authority_info.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    if confidential_transfer_mint_authority != *authority_info.key {
        return Err(TokenError::OwnerMismatch.into());
    }

    confidential_transfer_mint.auto_approve_new_accounts = auto_approve_new_account;
    confidential_transfer_mint.auditor_elgamal_pubkey = *auditor_encryption_pubkey;
    Ok(())
}

/// Processes a [ConfigureAccount] instruction.
fn process_configure_account(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    decryptable_zero_balance: &DecryptableBalance,
    maximum_pending_balance_credit_counter: &PodU64,
    proof_instruction_offset: i64,
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let token_account_info = next_account_info(account_info_iter)?;
    let mint_info = next_account_info(account_info_iter)?;

    // zero-knowledge proof certifies that the supplied ElGamal public key is valid
    let proof_context = verify_configure_account_proof(
        next_account_info(account_info_iter)?,
        proof_instruction_offset,
    )?;

    let authority_info = next_account_info(account_info_iter)?;
    let authority_info_data_len = authority_info.data_len();

    check_program_account(token_account_info.owner)?;
    let token_account_data = &mut token_account_info.data.borrow_mut();
    let mut token_account = StateWithExtensionsMut::<Account>::unpack(token_account_data)?;

    if token_account.base.mint != *mint_info.key {
        return Err(TokenError::MintMismatch.into());
    }

    Processor::validate_owner(
        program_id,
        &token_account.base.owner,
        authority_info,
        authority_info_data_len,
        account_info_iter.as_slice(),
    )?;

    check_program_account(mint_info.owner)?;
    let mint_data = &mut mint_info.data.borrow();
    let mint = StateWithExtensions::<Mint>::unpack(mint_data)?;
    let confidential_transfer_mint = mint.get_extension::<ConfidentialTransferMint>()?;

    // Note: The caller is expected to use the `Reallocate` instruction to ensure there is
    // sufficient room in their token account for the new `ConfidentialTransferAccount` extension
    let mut confidential_transfer_account =
        token_account.init_extension::<ConfidentialTransferAccount>(false)?;
    confidential_transfer_account.approved = confidential_transfer_mint.auto_approve_new_accounts;
    confidential_transfer_account.elgamal_pubkey = proof_context.pubkey;
    confidential_transfer_account.maximum_pending_balance_credit_counter =
        *maximum_pending_balance_credit_counter;

    // The all-zero ciphertext [0; 64] is a valid encryption of zero
    confidential_transfer_account.pending_balance_lo = EncryptedBalance::zeroed();
    confidential_transfer_account.pending_balance_hi = EncryptedBalance::zeroed();
    confidential_transfer_account.available_balance = EncryptedBalance::zeroed();

    confidential_transfer_account.decryptable_available_balance = *decryptable_zero_balance;
    confidential_transfer_account.allow_confidential_credits = true.into();
    confidential_transfer_account.pending_balance_credit_counter = 0.into();
    confidential_transfer_account.expected_pending_balance_credit_counter = 0.into();
    confidential_transfer_account.actual_pending_balance_credit_counter = 0.into();
    confidential_transfer_account.allow_non_confidential_credits = true.into();

    // if the mint is extended for fees, then initialize account for confidential transfer fees
    if mint.get_extension::<TransferFeeConfig>().is_ok() {
        let mut confidential_transfer_fee_amount =
            token_account.init_extension::<ConfidentialTransferFeeAmount>(false)?;
        confidential_transfer_fee_amount.withheld_amount = EncryptedWithheldAmount::zeroed();
    }

    Ok(())
}

/// Verify zero-knowledge proof needed for a [ConfigureAccount] instruction and return the
/// corresponding proof context.
fn verify_configure_account_proof(
    account_info: &AccountInfo<'_>,
    proof_instruction_offset: i64,
) -> Result<PubkeyValidityProofContext, ProgramError> {
    if proof_instruction_offset == 0 {
        // interpret `account_info` as a context state account
        check_zk_token_proof_program_account(account_info.owner)?;
        let context_state_account_data = account_info.data.borrow();
        let context_state = pod_from_bytes::<ProofContextState<PubkeyValidityProofContext>>(
            &context_state_account_data,
        )?;

        if context_state.proof_type != ProofType::PubkeyValidity.into() {
            return Err(ProgramError::InvalidInstructionData);
        }

        Ok(context_state.proof_context)
    } else {
        // interpret `account_info` as a sysvar
        let zkp_instruction = get_instruction_relative(proof_instruction_offset, account_info)?;
        Ok(*decode_proof_instruction_context::<
            PubkeyValidityData,
            PubkeyValidityProofContext,
        >(
            ProofInstruction::VerifyPubkeyValidity, &zkp_instruction
        )?)
    }
}

/// Processes an [ApproveAccount] instruction.
fn process_approve_account(accounts: &[AccountInfo]) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let token_account_info = next_account_info(account_info_iter)?;
    let mint_info = next_account_info(account_info_iter)?;
    let authority_info = next_account_info(account_info_iter)?;

    check_program_account(token_account_info.owner)?;
    let token_account_data = &mut token_account_info.data.borrow_mut();
    let mut token_account = StateWithExtensionsMut::<Account>::unpack(token_account_data)?;

    check_program_account(mint_info.owner)?;
    let mint_data = &mint_info.data.borrow_mut();
    let mint = StateWithExtensions::<Mint>::unpack(mint_data)?;
    let confidential_transfer_mint = mint.get_extension::<ConfidentialTransferMint>()?;
    let maybe_confidential_transfer_mint_authority: Option<Pubkey> =
        confidential_transfer_mint.authority.into();
    let confidential_transfer_mint_authority =
        maybe_confidential_transfer_mint_authority.ok_or(TokenError::NoAuthorityExists)?;

    if authority_info.is_signer && *authority_info.key == confidential_transfer_mint_authority {
        let mut confidential_transfer_state =
            token_account.get_extension_mut::<ConfidentialTransferAccount>()?;
        confidential_transfer_state.approved = true.into();
        Ok(())
    } else {
        Err(ProgramError::MissingRequiredSignature)
    }
}

/// Processes an [EmptyAccount] instruction.
fn process_empty_account(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    proof_instruction_offset: i64,
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let token_account_info = next_account_info(account_info_iter)?;

    // zero-knowledge proof certifies that the available balance ciphertext holds the balance of 0.
    let proof_context = verify_empty_account_proof(
        next_account_info(account_info_iter)?,
        proof_instruction_offset,
    )?;

    let authority_info = next_account_info(account_info_iter)?;
    let authority_info_data_len = authority_info.data_len();

    check_program_account(token_account_info.owner)?;
    let token_account_data = &mut token_account_info.data.borrow_mut();
    let mut token_account = StateWithExtensionsMut::<Account>::unpack(token_account_data)?;

    Processor::validate_owner(
        program_id,
        &token_account.base.owner,
        authority_info,
        authority_info_data_len,
        account_info_iter.as_slice(),
    )?;

    let mut confidential_transfer_account =
        token_account.get_extension_mut::<ConfidentialTransferAccount>()?;

    // Check that the encryption public key and ciphertext associated with the confidential
    // extension account are consistent with those that were actually used to generate the zkp.
    if confidential_transfer_account.elgamal_pubkey != proof_context.pubkey {
        msg!("Encryption public-key mismatch");
        return Err(TokenError::ConfidentialTransferElGamalPubkeyMismatch.into());
    }
    if confidential_transfer_account.available_balance != proof_context.ciphertext {
        msg!("Available balance mismatch");
        return Err(ProgramError::InvalidInstructionData);
    }
    confidential_transfer_account.available_balance = EncryptedBalance::zeroed();

    // check that all balances are all-zero ciphertexts
    confidential_transfer_account.closable()?;

    Ok(())
}

/// Verify zero-knowledge proof needed for a [EmptyAccount] instruction and return the
/// corresponding proof context.
fn verify_empty_account_proof(
    account_info: &AccountInfo<'_>,
    proof_instruction_offset: i64,
) -> Result<ZeroBalanceProofContext, ProgramError> {
    if proof_instruction_offset == 0 {
        // interpret `account_info` as a context state account
        check_zk_token_proof_program_account(account_info.owner)?;
        let context_state_account_data = account_info.data.borrow();
        let context_state = pod_from_bytes::<ProofContextState<ZeroBalanceProofContext>>(
            &context_state_account_data,
        )?;

        if context_state.proof_type != ProofType::ZeroBalance.into() {
            return Err(ProgramError::InvalidInstructionData);
        }

        Ok(context_state.proof_context)
    } else {
        // interpret `account_info` as a sysvar
        let zkp_instruction = get_instruction_relative(proof_instruction_offset, account_info)?;
        Ok(*decode_proof_instruction_context::<
            ZeroBalanceProofData,
            ZeroBalanceProofContext,
        >(
            ProofInstruction::VerifyZeroBalance, &zkp_instruction
        )?)
    }
}

/// Processes a [Deposit] instruction.
#[cfg(feature = "zk-ops")]
fn process_deposit(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    amount: u64,
    expected_decimals: u8,
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let token_account_info = next_account_info(account_info_iter)?;
    let mint_info = next_account_info(account_info_iter)?;
    let authority_info = next_account_info(account_info_iter)?;
    let authority_info_data_len = authority_info.data_len();

    check_program_account(mint_info.owner)?;
    let mint_data = &mint_info.data.borrow_mut();
    let mint = StateWithExtensions::<Mint>::unpack(mint_data)?;

    if expected_decimals != mint.base.decimals {
        return Err(TokenError::MintDecimalsMismatch.into());
    }

    if mint.get_extension::<NonTransferable>().is_ok() {
        return Err(TokenError::NonTransferable.into());
    }

    check_program_account(token_account_info.owner)?;
    let token_account_data = &mut token_account_info.data.borrow_mut();
    let mut token_account = StateWithExtensionsMut::<Account>::unpack(token_account_data)?;

    Processor::validate_owner(
        program_id,
        &token_account.base.owner,
        authority_info,
        authority_info_data_len,
        account_info_iter.as_slice(),
    )?;

    if token_account.base.is_frozen() {
        return Err(TokenError::AccountFrozen.into());
    }

    if token_account.base.mint != *mint_info.key {
        return Err(TokenError::MintMismatch.into());
    }

    // Wrapped SOL deposits are not supported because lamports cannot be vanished.
    assert!(!token_account.base.is_native());

    token_account.base.amount = token_account
        .base
        .amount
        .checked_sub(amount)
        .ok_or(TokenError::Overflow)?;
    token_account.pack_base();

    let mut confidential_transfer_account =
        token_account.get_extension_mut::<ConfidentialTransferAccount>()?;
    confidential_transfer_account.valid_as_destination()?;

    // A deposit amount must be a 48-bit number
    let (amount_lo, amount_hi) = verify_and_split_deposit_amount(amount)?;

    // Prevent unnecessary ciphertext arithmetic syscalls if `amount_lo` or `amount_hi` is zero
    if amount_lo > 0 {
        confidential_transfer_account.pending_balance_lo =
            syscall::add_to(&confidential_transfer_account.pending_balance_lo, amount_lo)
                .ok_or(ProgramError::InvalidInstructionData)?;
    }
    if amount_hi > 0 {
        confidential_transfer_account.pending_balance_hi =
            syscall::add_to(&confidential_transfer_account.pending_balance_hi, amount_hi)
                .ok_or(ProgramError::InvalidInstructionData)?;
    }

    confidential_transfer_account.increment_pending_balance_credit_counter()?;

    Ok(())
}

/// Verifies that a deposit amount is a 48-bit number and returns the least significant 16 bits and
/// most significant 32 bits of the amount.
#[cfg(feature = "zk-ops")]
fn verify_and_split_deposit_amount(amount: u64) -> Result<(u64, u64), TokenError> {
    if amount > MAXIMUM_DEPOSIT_TRANSFER_AMOUNT {
        return Err(TokenError::MaximumDepositAmountExceeded);
    }
    let deposit_amount_lo = amount & (u16::MAX as u64);
    let deposit_amount_hi = amount.checked_shr(u16::BITS).unwrap();
    Ok((deposit_amount_lo, deposit_amount_hi))
}

/// Processes a [Withdraw] instruction.
#[cfg(feature = "zk-ops")]
fn process_withdraw(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    amount: u64,
    expected_decimals: u8,
    new_decryptable_available_balance: DecryptableBalance,
    proof_instruction_offset: i64,
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let token_account_info = next_account_info(account_info_iter)?;
    let mint_info = next_account_info(account_info_iter)?;

    // zero-knowledge proof certifies that the account has enough available balance to withdraw the
    // amount.
    let proof_context = verify_withdraw_proof(
        next_account_info(account_info_iter)?,
        proof_instruction_offset,
    )?;

    let authority_info = next_account_info(account_info_iter)?;
    let authority_info_data_len = authority_info.data_len();

    check_program_account(mint_info.owner)?;
    let mint_data = &mint_info.data.borrow_mut();
    let mint = StateWithExtensions::<Mint>::unpack(mint_data)?;

    if expected_decimals != mint.base.decimals {
        return Err(TokenError::MintDecimalsMismatch.into());
    }

    if mint.get_extension::<NonTransferable>().is_ok() {
        return Err(TokenError::NonTransferable.into());
    }

    check_program_account(token_account_info.owner)?;
    let token_account_data = &mut token_account_info.data.borrow_mut();
    let mut token_account = StateWithExtensionsMut::<Account>::unpack(token_account_data)?;

    Processor::validate_owner(
        program_id,
        &token_account.base.owner,
        authority_info,
        authority_info_data_len,
        account_info_iter.as_slice(),
    )?;

    if token_account.base.is_frozen() {
        return Err(TokenError::AccountFrozen.into());
    }

    if token_account.base.mint != *mint_info.key {
        return Err(TokenError::MintMismatch.into());
    }

    // Wrapped SOL withdrawals are not supported because lamports cannot be apparated.
    assert!(!token_account.base.is_native());

    let mut confidential_transfer_account =
        token_account.get_extension_mut::<ConfidentialTransferAccount>()?;
    confidential_transfer_account.valid_as_source()?;

    // Check that the encryption public key associated with the confidential extension is
    // consistent with the public key that was actually used to generate the zkp.
    if confidential_transfer_account.elgamal_pubkey != proof_context.pubkey {
        return Err(TokenError::ConfidentialTransferElGamalPubkeyMismatch.into());
    }

    // Prevent unnecessary ciphertext arithmetic syscalls if the withdraw amount is zero
    if amount > 0 {
        confidential_transfer_account.available_balance =
            syscall::subtract_from(&confidential_transfer_account.available_balance, amount)
                .ok_or(ProgramError::InvalidInstructionData)?;
    }
    // Check that the final available balance ciphertext is consistent with the actual ciphertext
    // for which the zero-knowledge proof was generated for.
    if confidential_transfer_account.available_balance != proof_context.final_ciphertext {
        return Err(TokenError::ConfidentialTransferBalanceMismatch.into());
    }

    confidential_transfer_account.decryptable_available_balance = new_decryptable_available_balance;
    token_account.base.amount = token_account
        .base
        .amount
        .checked_add(amount)
        .ok_or(TokenError::Overflow)?;
    token_account.pack_base();

    Ok(())
}

/// Verify zero-knowledge proof needed for a [Withdraw] instruction and return the
/// corresponding proof context.
fn verify_withdraw_proof(
    account_info: &AccountInfo<'_>,
    proof_instruction_offset: i64,
) -> Result<WithdrawProofContext, ProgramError> {
    if proof_instruction_offset == 0 {
        // interpret `account_info` as a context state account
        check_zk_token_proof_program_account(account_info.owner)?;
        let context_state_account_data = account_info.data.borrow();
        let context_state =
            pod_from_bytes::<ProofContextState<WithdrawProofContext>>(&context_state_account_data)?;

        if context_state.proof_type != ProofType::Withdraw.into() {
            return Err(ProgramError::InvalidInstructionData);
        }

        Ok(context_state.proof_context)
    } else {
        // interpret `account_info` as a sysvar
        let zkp_instruction = get_instruction_relative(proof_instruction_offset, account_info)?;
        Ok(*decode_proof_instruction_context::<
            WithdrawData,
            WithdrawProofContext,
        >(
            ProofInstruction::VerifyWithdraw, &zkp_instruction
        )?)
    }
}

/// Processes an [Transfer] instruction.
#[cfg(feature = "zk-ops")]
fn process_transfer(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    new_source_decryptable_available_balance: DecryptableBalance,
    proof_instruction_offset: i64,
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let source_account_info = next_account_info(account_info_iter)?;
    let destination_token_account_info = next_account_info(account_info_iter)?;
    let mint_info = next_account_info(account_info_iter)?;

    // either sysvar or context state account depending on `proof_instruction_offset`
    let proof_account_info = next_account_info(account_info_iter)?;

    let authority_info = next_account_info(account_info_iter)?;

    check_program_account(mint_info.owner)?;
    let mint_data = &mint_info.data.borrow_mut();
    let mint = StateWithExtensions::<Mint>::unpack(mint_data)?;

    if mint.get_extension::<NonTransferable>().is_ok() {
        return Err(TokenError::NonTransferable.into());
    }
    let confidential_transfer_mint = mint.get_extension::<ConfidentialTransferMint>()?;

    // A `Transfer` instruction must be accompanied by a zero-knowledge proof instruction that
    // certify the validity of the transfer amounts. The kind of zero-knowledge proof instruction
    // depends on whether a transfer incurs a fee or not.
    //   - If the mint is not extended for fees or the instruction is for a self-transfer, then
    //   transfer fee is not required.
    //   - If the mint is extended for fees and the instruction is not a self-transfer, then
    //   transfer fee is required.
    if mint.get_extension::<TransferFeeConfig>().is_err()
        || source_account_info.key == destination_token_account_info.key
    {
        // Transfer fee is not required. Decode the zero-knowledge proof as `TransferData`.
        //
        // The zero-knowledge proof certifies that:
        //   1. the transfer amount is encrypted in the correct form
        //   2. the source account has enough balance to send the transfer amount
        let proof_context = verify_transfer_proof(proof_account_info, proof_instruction_offset)?;

        // Check that the auditor encryption public key associated wth the confidential mint is
        // consistent with what was actually used to generate the zkp.
        if !confidential_transfer_mint
            .auditor_elgamal_pubkey
            .equals(&proof_context.transfer_pubkeys.auditor)
        {
            return Err(TokenError::ConfidentialTransferElGamalPubkeyMismatch.into());
        }

        let source_transfer_amount_lo =
            transfer_amount_source_ciphertext(&proof_context.ciphertext_lo);
        let source_transfer_amount_hi =
            transfer_amount_source_ciphertext(&proof_context.ciphertext_hi);

        process_source_for_transfer(
            program_id,
            source_account_info,
            mint_info,
            authority_info,
            account_info_iter.as_slice(),
            &proof_context.transfer_pubkeys.source,
            &source_transfer_amount_lo,
            &source_transfer_amount_hi,
            &proof_context.new_source_ciphertext,
            new_source_decryptable_available_balance,
        )?;

        let destination_ciphertext_lo =
            transfer_amount_destination_ciphertext(&proof_context.ciphertext_lo);
        let destination_ciphertext_hi =
            transfer_amount_destination_ciphertext(&proof_context.ciphertext_hi);

        process_destination_for_transfer(
            destination_token_account_info,
            mint_info,
            &proof_context.transfer_pubkeys.destination,
            &destination_ciphertext_lo,
            &destination_ciphertext_hi,
            None,
        )?;
    } else {
        // Transfer fee is required. Decode the zero-knowledge proof as `TransferWithFeeData`.
        //
        // The zero-knowledge proof certifies that:
        //   1. the transfer amount is encrypted in the correct form
        //   2. the source account has enough balance to send the transfer amount
        //   3. the transfer fee is computed correctly and encrypted in the correct form
        let proof_context =
            verify_transfer_with_fee_proof(proof_account_info, proof_instruction_offset)?;

        // Check that the encryption public keys associated with the mint confidential transfer and
        // confidential transfer fee extensions are consistent with the keys that were used to
        // generate the zkp.
        if !confidential_transfer_mint
            .auditor_elgamal_pubkey
            .equals(&proof_context.transfer_with_fee_pubkeys.auditor)
        {
            return Err(TokenError::ConfidentialTransferElGamalPubkeyMismatch.into());
        }
        let confidential_transfer_fee_config =
            mint.get_extension::<ConfidentialTransferFeeConfig>()?;
        if proof_context
            .transfer_with_fee_pubkeys
            .withdraw_withheld_authority
            != confidential_transfer_fee_config.withdraw_withheld_authority_elgamal_pubkey
        {
            return Err(TokenError::ConfidentialTransferElGamalPubkeyMismatch.into());
        }

        // Check that the fee parameters in the mint are consistent with what were used to generate
        // the zkp.
        let transfer_fee_config = mint.get_extension::<TransferFeeConfig>()?;
        let fee_parameters = transfer_fee_config.get_epoch_fee(Clock::get()?.epoch);
        if u64::from(fee_parameters.maximum_fee)
            != u64::from(proof_context.fee_parameters.maximum_fee)
            || u16::from(fee_parameters.transfer_fee_basis_points)
                != u16::from(proof_context.fee_parameters.fee_rate_basis_points)
        {
            return Err(TokenError::FeeParametersMismatch.into());
        }

        // From the proof data, decode lo and hi transfer amounts encrypted under the source
        // encryption public key
        let source_transfer_amount_lo =
            transfer_amount_source_ciphertext(&proof_context.ciphertext_lo);
        let source_transfer_amount_hi =
            transfer_amount_source_ciphertext(&proof_context.ciphertext_hi);

        process_source_for_transfer(
            program_id,
            source_account_info,
            mint_info,
            authority_info,
            account_info_iter.as_slice(),
            &proof_context.transfer_with_fee_pubkeys.source,
            &source_transfer_amount_lo,
            &source_transfer_amount_hi,
            &proof_context.new_source_ciphertext,
            new_source_decryptable_available_balance,
        )?;

        // From the proof data decode lo and hi transfer amounts encrypted under the destination
        // encryption public key
        let destination_transfer_amount_lo =
            transfer_amount_destination_ciphertext(&proof_context.ciphertext_lo);
        let destination_transfer_amount_hi =
            transfer_amount_destination_ciphertext(&proof_context.ciphertext_hi);

        process_destination_for_transfer(
            destination_token_account_info,
            mint_info,
            &proof_context.transfer_with_fee_pubkeys.destination,
            &destination_transfer_amount_lo,
            &destination_transfer_amount_hi,
            Some((
                &proof_context.fee_ciphertext_lo,
                &proof_context.fee_ciphertext_hi,
            )),
        )?;
    }

    Ok(())
}

/// Verify zero-knowledge proof needed for a [Transfer] instruction without fee and return the
/// corresponding proof context.
fn verify_transfer_proof(
    account_info: &AccountInfo<'_>,
    proof_instruction_offset: i64,
) -> Result<TransferProofContext, ProgramError> {
    if proof_instruction_offset == 0 {
        // interpret `account_info` as a context state account
        check_zk_token_proof_program_account(account_info.owner)?;
        let context_state_account_data = account_info.data.borrow();
        let context_state =
            pod_from_bytes::<ProofContextState<TransferProofContext>>(&context_state_account_data)?;

        if context_state.proof_type != ProofType::Transfer.into() {
            return Err(ProgramError::InvalidInstructionData);
        }

        Ok(context_state.proof_context)
    } else {
        // interpret `account_info` as a sysvar
        let zkp_instruction = get_instruction_relative(proof_instruction_offset, account_info)?;
        Ok(*decode_proof_instruction_context::<
            TransferData,
            TransferProofContext,
        >(
            ProofInstruction::VerifyTransfer, &zkp_instruction
        )?)
    }
}

/// Verify zero-knowledge proof needed for a [Transfer] instruction with fee and return the
/// corresponding proof context.
fn verify_transfer_with_fee_proof(
    account_info: &AccountInfo<'_>,
    proof_instruction_offset: i64,
) -> Result<TransferWithFeeProofContext, ProgramError> {
    if proof_instruction_offset == 0 {
        // interpret `account_info` as a context state account
        check_zk_token_proof_program_account(account_info.owner)?;
        let context_state_account_data = account_info.data.borrow();
        let context_state = pod_from_bytes::<ProofContextState<TransferWithFeeProofContext>>(
            &context_state_account_data,
        )?;

        if context_state.proof_type != ProofType::TransferWithFee.into() {
            return Err(ProgramError::InvalidInstructionData);
        }

        Ok(context_state.proof_context)
    } else {
        // interpret `account_info` as a sysvar
        let zkp_instruction = get_instruction_relative(proof_instruction_offset, account_info)?;
        Ok(*decode_proof_instruction_context::<
            TransferWithFeeData,
            TransferWithFeeProofContext,
        >(
            ProofInstruction::VerifyTransferWithFee,
            &zkp_instruction,
        )?)
    }
}

/// Extract the transfer amount ciphertext encrypted under the source ElGamal public key.
///
/// A transfer amount ciphertext consists of the following 32-byte components that are serialized
/// in order:
///   1. The `commitment` component that encodes the transfer amount.
///   2. The `decryption handle` component with respect to the source public key.
///   3. The `decryption handle` component with respect to the destination public key.
///   4. The `decryption handle` component with respect to the auditor public key.
///
/// An ElGamal ciphertext for the source consists of the `commitment` component and the `decryption
/// handle` component with respect to the source.
#[cfg(feature = "zk-ops")]
fn transfer_amount_source_ciphertext(
    transfer_amount_ciphertext: &TransferAmountCiphertext,
) -> ElGamalCiphertext {
    let transfer_amount_ciphertext_bytes = bytemuck::bytes_of(transfer_amount_ciphertext);

    let mut source_ciphertext_bytes = [0u8; 64];
    source_ciphertext_bytes[..32].copy_from_slice(&transfer_amount_ciphertext_bytes[..32]);
    source_ciphertext_bytes[32..].copy_from_slice(&transfer_amount_ciphertext_bytes[32..64]);

    ElGamalCiphertext(source_ciphertext_bytes)
}

/// Extract the transfer amount ciphertext encrypted under the destination ElGamal public key.
///
/// A transfer amount ciphertext consists of the following 32-byte components that are serialized
/// in order:
///   1. The `commitment` component that encodes the transfer amount.
///   2. The `decryption handle` component with respect to the source public key.
///   3. The `decryption handle` component with respect to the destination public key.
///   4. The `decryption handle` component with respect to the auditor public key.
///
/// An ElGamal ciphertext for the destination consists of the `commitment` component and the
/// `decryption handle` component with respect to the destination public key.
#[cfg(feature = "zk-ops")]
fn transfer_amount_destination_ciphertext(
    transfer_amount_ciphertext: &TransferAmountCiphertext,
) -> ElGamalCiphertext {
    let transfer_amount_ciphertext_bytes = bytemuck::bytes_of(transfer_amount_ciphertext);

    let mut destination_ciphertext_bytes = [0u8; 64];
    destination_ciphertext_bytes[..32].copy_from_slice(&transfer_amount_ciphertext_bytes[..32]);
    destination_ciphertext_bytes[32..].copy_from_slice(&transfer_amount_ciphertext_bytes[64..96]);

    ElGamalCiphertext(destination_ciphertext_bytes)
}

/// Extract the fee amount ciphertext encrypted under the destination ElGamal public key.
///
/// A fee encryption amount consists of the following 32-byte components that are serialized in
/// order:
///   1. The `commitment` component that encodes the fee amount.
///   2. The `decryption handle` component with respect to the destination public key.
///   3. The `decryption handle` component with respect to the withdraw withheld authority public
///      key.
///
/// An ElGamal ciphertext for the destination consists of the `commitment` component and the
/// `decryption handle` component with respect to the destination public key.
#[cfg(feature = "zk-ops")]
fn fee_amount_destination_ciphertext(
    transfer_amount_ciphertext: &EncryptedFee,
) -> ElGamalCiphertext {
    let transfer_amount_ciphertext_bytes = bytemuck::bytes_of(transfer_amount_ciphertext);

    let mut source_ciphertext_bytes = [0u8; 64];
    source_ciphertext_bytes[..32].copy_from_slice(&transfer_amount_ciphertext_bytes[..32]);
    source_ciphertext_bytes[32..].copy_from_slice(&transfer_amount_ciphertext_bytes[32..64]);

    ElGamalCiphertext(source_ciphertext_bytes)
}

/// Extract the transfer amount ciphertext encrypted under the withdraw withheld authority ElGamal
/// public key.
///
/// A fee encryption amount consists of the following 32-byte components that are serialized in
/// order:
///   1. The `commitment` component that encodes the fee amount.
///   2. The `decryption handle` component with respect to the destination public key.
///   3. The `decryption handle` component with respect to the withdraw withheld authority public
///      key.
///
/// An ElGamal ciphertext for the destination consists of the `commitment` component and the
/// `decryption handle` component with respect to the withdraw withheld authority public key.
#[cfg(feature = "zk-ops")]
fn fee_amount_withdraw_withheld_authority_ciphertext(
    transfer_amount_ciphertext: &EncryptedFee,
) -> ElGamalCiphertext {
    let transfer_amount_ciphertext_bytes = bytemuck::bytes_of(transfer_amount_ciphertext);

    let mut destination_ciphertext_bytes = [0u8; 64];
    destination_ciphertext_bytes[..32].copy_from_slice(&transfer_amount_ciphertext_bytes[..32]);
    destination_ciphertext_bytes[32..].copy_from_slice(&transfer_amount_ciphertext_bytes[64..96]);

    ElGamalCiphertext(destination_ciphertext_bytes)
}

#[allow(clippy::too_many_arguments)]
#[cfg(feature = "zk-ops")]
fn process_source_for_transfer(
    program_id: &Pubkey,
    source_account_info: &AccountInfo,
    mint_info: &AccountInfo,
    authority_info: &AccountInfo,
    signers: &[AccountInfo],
    source_encryption_pubkey: &ElGamalPubkey,
    source_transfer_amount_lo: &EncryptedBalance,
    source_transfer_amount_hi: &EncryptedBalance,
    expected_new_source_available_balance: &EncryptedBalance,
    new_source_decryptable_available_balance: DecryptableBalance,
) -> ProgramResult {
    check_program_account(source_account_info.owner)?;
    let authority_info_data_len = authority_info.data_len();
    let token_account_data = &mut source_account_info.data.borrow_mut();
    let mut token_account = StateWithExtensionsMut::<Account>::unpack(token_account_data)?;

    Processor::validate_owner(
        program_id,
        &token_account.base.owner,
        authority_info,
        authority_info_data_len,
        signers,
    )?;

    if token_account.base.is_frozen() {
        return Err(TokenError::AccountFrozen.into());
    }

    if token_account.base.mint != *mint_info.key {
        return Err(TokenError::MintMismatch.into());
    }

    let mut confidential_transfer_account =
        token_account.get_extension_mut::<ConfidentialTransferAccount>()?;
    confidential_transfer_account.valid_as_source()?;

    // Check that the source encryption public key is consistent with what was actually used to
    // generate the zkp.
    if *source_encryption_pubkey != confidential_transfer_account.elgamal_pubkey {
        return Err(TokenError::ConfidentialTransferElGamalPubkeyMismatch.into());
    }

    let new_source_available_balance = syscall::subtract_with_lo_hi(
        &confidential_transfer_account.available_balance,
        source_transfer_amount_lo,
        source_transfer_amount_hi,
    )
    .ok_or(ProgramError::InvalidInstructionData)?;

    // Check that the computed available balance is consistent with what was actually used to
    // generate the zkp on the client side.
    if new_source_available_balance != *expected_new_source_available_balance {
        return Err(TokenError::ConfidentialTransferBalanceMismatch.into());
    }

    confidential_transfer_account.available_balance = new_source_available_balance;
    confidential_transfer_account.decryptable_available_balance =
        new_source_decryptable_available_balance;

    Ok(())
}

#[cfg(feature = "zk-ops")]
fn process_destination_for_transfer(
    destination_token_account_info: &AccountInfo,
    mint_info: &AccountInfo,
    destination_encryption_pubkey: &ElGamalPubkey,
    destination_transfer_amount_lo: &EncryptedBalance,
    destination_transfer_amount_hi: &EncryptedBalance,
    encrypted_fee: Option<(&EncryptedFee, &EncryptedFee)>,
) -> ProgramResult {
    check_program_account(destination_token_account_info.owner)?;
    let destination_token_account_data = &mut destination_token_account_info.data.borrow_mut();
    let mut destination_token_account =
        StateWithExtensionsMut::<Account>::unpack(destination_token_account_data)?;

    if destination_token_account.base.is_frozen() {
        return Err(TokenError::AccountFrozen.into());
    }

    if destination_token_account.base.mint != *mint_info.key {
        return Err(TokenError::MintMismatch.into());
    }

    if memo_required(&destination_token_account) {
        check_previous_sibling_instruction_is_memo()?;
    }

    let mut destination_confidential_transfer_account =
        destination_token_account.get_extension_mut::<ConfidentialTransferAccount>()?;
    destination_confidential_transfer_account.valid_as_destination()?;

    if *destination_encryption_pubkey != destination_confidential_transfer_account.elgamal_pubkey {
        return Err(TokenError::ConfidentialTransferElGamalPubkeyMismatch.into());
    }

    destination_confidential_transfer_account.pending_balance_lo = syscall::add(
        &destination_confidential_transfer_account.pending_balance_lo,
        destination_transfer_amount_lo,
    )
    .ok_or(ProgramError::InvalidInstructionData)?;

    destination_confidential_transfer_account.pending_balance_hi = syscall::add(
        &destination_confidential_transfer_account.pending_balance_hi,
        destination_transfer_amount_hi,
    )
    .ok_or(ProgramError::InvalidInstructionData)?;

    destination_confidential_transfer_account.increment_pending_balance_credit_counter()?;

    // Process transfer fee
    if let Some((ciphertext_fee_lo, ciphertext_fee_hi)) = encrypted_fee {
        // Decode lo and hi fee amounts encrypted under the destination encryption public key
        let destination_fee_lo = fee_amount_destination_ciphertext(ciphertext_fee_lo);
        let destination_fee_hi = fee_amount_destination_ciphertext(ciphertext_fee_hi);

        // Subtract the fee amount from the destination pending balance
        destination_confidential_transfer_account.pending_balance_lo = syscall::subtract(
            &destination_confidential_transfer_account.pending_balance_lo,
            &destination_fee_lo,
        )
        .ok_or(ProgramError::InvalidInstructionData)?;
        destination_confidential_transfer_account.pending_balance_hi = syscall::subtract(
            &destination_confidential_transfer_account.pending_balance_hi,
            &destination_fee_hi,
        )
        .ok_or(ProgramError::InvalidInstructionData)?;

        // Decode lo and hi fee amounts encrypted under the withdraw authority encryption public
        // key
        let withdraw_withheld_authority_fee_lo =
            fee_amount_withdraw_withheld_authority_ciphertext(ciphertext_fee_lo);
        let withdraw_withheld_authority_fee_hi =
            fee_amount_withdraw_withheld_authority_ciphertext(ciphertext_fee_hi);

        let mut destination_confidential_transfer_fee_amount =
            destination_token_account.get_extension_mut::<ConfidentialTransferFeeAmount>()?;

        // Add the fee amount to the destination withheld fee
        destination_confidential_transfer_fee_amount.withheld_amount = syscall::add_with_lo_hi(
            &destination_confidential_transfer_fee_amount.withheld_amount,
            &withdraw_withheld_authority_fee_lo,
            &withdraw_withheld_authority_fee_hi,
        )
        .ok_or(ProgramError::InvalidInstructionData)?;
    }

    Ok(())
}

/// Processes an [ApplyPendingBalance] instruction.
#[cfg(feature = "zk-ops")]
fn process_apply_pending_balance(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    ApplyPendingBalanceData {
        expected_pending_balance_credit_counter,
        new_decryptable_available_balance,
    }: &ApplyPendingBalanceData,
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let token_account_info = next_account_info(account_info_iter)?;
    let authority_info = next_account_info(account_info_iter)?;
    let authority_info_data_len = authority_info.data_len();

    check_program_account(token_account_info.owner)?;
    let token_account_data = &mut token_account_info.data.borrow_mut();
    let mut token_account = StateWithExtensionsMut::<Account>::unpack(token_account_data)?;

    Processor::validate_owner(
        program_id,
        &token_account.base.owner,
        authority_info,
        authority_info_data_len,
        account_info_iter.as_slice(),
    )?;

    let mut confidential_transfer_account =
        token_account.get_extension_mut::<ConfidentialTransferAccount>()?;

    confidential_transfer_account.available_balance = syscall::add_with_lo_hi(
        &confidential_transfer_account.available_balance,
        &confidential_transfer_account.pending_balance_lo,
        &confidential_transfer_account.pending_balance_hi,
    )
    .ok_or(ProgramError::InvalidInstructionData)?;

    confidential_transfer_account.actual_pending_balance_credit_counter =
        confidential_transfer_account.pending_balance_credit_counter;
    confidential_transfer_account.expected_pending_balance_credit_counter =
        *expected_pending_balance_credit_counter;
    confidential_transfer_account.decryptable_available_balance =
        *new_decryptable_available_balance;
    confidential_transfer_account.pending_balance_credit_counter = 0.into();
    confidential_transfer_account.pending_balance_lo = EncryptedBalance::zeroed();
    confidential_transfer_account.pending_balance_hi = EncryptedBalance::zeroed();

    Ok(())
}

/// Processes a [DisableConfidentialCredits] or [EnableConfidentialCredits] instruction.
fn process_allow_confidential_credits(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    allow_confidential_credits: bool,
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let token_account_info = next_account_info(account_info_iter)?;
    let authority_info = next_account_info(account_info_iter)?;
    let authority_info_data_len = authority_info.data_len();

    check_program_account(token_account_info.owner)?;
    let token_account_data = &mut token_account_info.data.borrow_mut();
    let mut token_account = StateWithExtensionsMut::<Account>::unpack(token_account_data)?;

    Processor::validate_owner(
        program_id,
        &token_account.base.owner,
        authority_info,
        authority_info_data_len,
        account_info_iter.as_slice(),
    )?;

    let mut confidential_transfer_account =
        token_account.get_extension_mut::<ConfidentialTransferAccount>()?;
    confidential_transfer_account.allow_confidential_credits = allow_confidential_credits.into();

    Ok(())
}

/// Processes an [DisableNonConfidentialCredits] or [EnableNonConfidentialCredits] instruction.
fn process_allow_non_confidential_credits(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    allow_non_confidential_credits: bool,
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let token_account_info = next_account_info(account_info_iter)?;
    let authority_info = next_account_info(account_info_iter)?;
    let authority_info_data_len = authority_info.data_len();

    check_program_account(token_account_info.owner)?;
    let token_account_data = &mut token_account_info.data.borrow_mut();
    let mut token_account = StateWithExtensionsMut::<Account>::unpack(token_account_data)?;

    Processor::validate_owner(
        program_id,
        &token_account.base.owner,
        authority_info,
        authority_info_data_len,
        account_info_iter.as_slice(),
    )?;

    let mut confidential_transfer_account =
        token_account.get_extension_mut::<ConfidentialTransferAccount>()?;
    confidential_transfer_account.allow_non_confidential_credits =
        allow_non_confidential_credits.into();

    Ok(())
}

#[allow(dead_code)]
pub(crate) fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    input: &[u8],
) -> ProgramResult {
    check_program_account(program_id)?;

    match decode_instruction_type(input)? {
        ConfidentialTransferInstruction::InitializeMint => {
            msg!("ConfidentialTransferInstruction::InitializeMint");
            let data = decode_instruction_data::<InitializeMintData>(input)?;
            process_initialize_mint(
                accounts,
                &data.authority,
                data.auto_approve_new_accounts,
                &data.auditor_elgamal_pubkey,
            )
        }
        ConfidentialTransferInstruction::UpdateMint => {
            msg!("ConfidentialTransferInstruction::UpdateMint");
            let data = decode_instruction_data::<UpdateMintData>(input)?;
            process_update_mint(
                accounts,
                data.auto_approve_new_accounts,
                &data.auditor_elgamal_pubkey,
            )
        }
        ConfidentialTransferInstruction::ConfigureAccount => {
            msg!("ConfidentialTransferInstruction::ConfigureAccount");
            let data = decode_instruction_data::<ConfigureAccountInstructionData>(input)?;
            process_configure_account(
                program_id,
                accounts,
                &data.decryptable_zero_balance,
                &data.maximum_pending_balance_credit_counter,
                data.proof_instruction_offset as i64,
            )
        }
        ConfidentialTransferInstruction::ApproveAccount => {
            msg!("ConfidentialTransferInstruction::ApproveAccount");
            process_approve_account(accounts)
        }
        ConfidentialTransferInstruction::EmptyAccount => {
            msg!("ConfidentialTransferInstruction::EmptyAccount");
            let data = decode_instruction_data::<EmptyAccountInstructionData>(input)?;
            process_empty_account(program_id, accounts, data.proof_instruction_offset as i64)
        }
        ConfidentialTransferInstruction::Deposit => {
            msg!("ConfidentialTransferInstruction::Deposit");
            #[cfg(feature = "zk-ops")]
            {
                let data = decode_instruction_data::<DepositInstructionData>(input)?;
                process_deposit(program_id, accounts, data.amount.into(), data.decimals)
            }
            #[cfg(not(feature = "zk-ops"))]
            Err(ProgramError::InvalidInstructionData)
        }
        ConfidentialTransferInstruction::Withdraw => {
            msg!("ConfidentialTransferInstruction::Withdraw");
            #[cfg(feature = "zk-ops")]
            {
                let data = decode_instruction_data::<WithdrawInstructionData>(input)?;
                process_withdraw(
                    program_id,
                    accounts,
                    data.amount.into(),
                    data.decimals,
                    data.new_decryptable_available_balance,
                    data.proof_instruction_offset as i64,
                )
            }
        }
        ConfidentialTransferInstruction::Transfer => {
            msg!("ConfidentialTransferInstruction::Transfer");
            #[cfg(feature = "zk-ops")]
            let data = decode_instruction_data::<TransferInstructionData>(input)?;
            process_transfer(
                program_id,
                accounts,
                data.new_source_decryptable_available_balance,
                data.proof_instruction_offset as i64,
            )
        }
        ConfidentialTransferInstruction::ApplyPendingBalance => {
            msg!("ConfidentialTransferInstruction::ApplyPendingBalance");
            #[cfg(feature = "zk-ops")]
            {
                process_apply_pending_balance(
                    program_id,
                    accounts,
                    decode_instruction_data::<ApplyPendingBalanceData>(input)?,
                )
            }
            #[cfg(not(feature = "zk-ops"))]
            {
                Err(ProgramError::InvalidInstructionData)
            }
        }
        ConfidentialTransferInstruction::DisableConfidentialCredits => {
            msg!("ConfidentialTransferInstruction::DisableConfidentialCredits");
            process_allow_confidential_credits(program_id, accounts, false)
        }
        ConfidentialTransferInstruction::EnableConfidentialCredits => {
            msg!("ConfidentialTransferInstruction::EnableConfidentialCredits");
            process_allow_confidential_credits(program_id, accounts, true)
        }
        ConfidentialTransferInstruction::DisableNonConfidentialCredits => {
            msg!("ConfidentialTransferInstruction::DisableNonConfidentialCredits");
            process_allow_non_confidential_credits(program_id, accounts, false)
        }
        ConfidentialTransferInstruction::EnableNonConfidentialCredits => {
            msg!("ConfidentialTransferInstruction::EnableNonConfidentialCredits");
            process_allow_non_confidential_credits(program_id, accounts, true)
        }
    }
}
