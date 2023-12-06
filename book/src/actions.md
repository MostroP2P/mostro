# Actions

## mostro_core::Action

Action is used to identify each message between Mostro and users

```rust
  pub enum Action {
    Order,
    TakeSell,
    TakeBuy,
    PayInvoice,
    FiatSent,
    Release,
    Cancel,
    CooperativeCancelInitiatedByYou,
    CooperativeCancelInitiatedByPeer,
    DisputeInitiatedByYou,
    DisputeInitiatedByPeer,
    CooperativeCancelAccepted,
    BuyerInvoiceAccepted,
    SaleCompleted,
    PurchaseCompleted,
    HoldInvoicePaymentAccepted,
    HoldInvoicePaymentSettled,
    HoldInvoicePaymentCanceled,
    WaitingSellerToPay,
    WaitingBuyerInvoice,
    AddInvoice,
    BuyerTookOrder,
    RateUser,
    CantDo,
    Received,
    Dispute,
    AdminCancel,
    AdminSettle,
    AdminAddSolver,
    AdminTakeDispute,
}
```

You can see details in [mostro core documentation](https://docs.rs/mostro-core/latest/mostro_core/message/enum.Action.html)
