# Message suggestions for some actions

Here are suggestions for messages that clients could show to users when they receive certain actions. Clients can customize these messages to their convenience, as well as translate them, add emojis or any other modifications they consider necessary to provide a good user experience. Clients must complete the information that is in `monospace` format.

- **new-order:** Your offer has been published! Please wait until another user picks your order. It will be available for `expiration_hours` hours. You can cancel this order before another user picks it up by executing: `cancel`.

- **canceled:** You have cancelled the order ID: `id`!

- **pay-invoice:** Please pay this hold invoice of `amount` Sats for `fiat_code` `fiat_amount` to start the operation. If you do not pay it within `expiration_seconds` the trade will be cancelled.

- **add-invoice:** Please send me an invoice for `amount` satoshis equivalent to  `fiat_code` `fiat_amount`.  This is where I’ll send the funds upon completion of the trade. If you don't provide the invoice within `expiration_seconds` this trade will be cancelled.

- **waiting-seller-to-pay:** Please wait a bit. I've sent a payment request to the seller to sends the Sats for the order ID `id`. Once the payment is made, I'll connect you both. If the seller doesn’t complete the payment within `expiration_seconds` minutes the trade will be cancelled.

- **waiting-buyer-invoice:** Payment received! Your Sats are now "held" in your own wallet. Please wait a bit. I've requested the buyer to provide an invoice. Once they do, I 'll connect you both. If they does not do so within `expiration_seconds` your Sats will be available at your wallet again and the trade will be cancelled.

- **buyer-invoice-accepted:** Invoice has been successfully saved!

- **hold-invoice-payment-accepted:** Get in touch with the seller, this is their npub `seller-npub` to get the details on how to send the fiat money for the order `id`, you must send `fiat_code` `fiat_amount` using `payment_method`. Once you send the fiat money, please let me know with `fiat-sent`.

- **buyer-took-order:** Get in touch with the buyer, this is their npub `buyer-npub` to inform them how to send you `fiat_code` `fiat_amount` through `payment_method`. I will notify you once the buyer indicates the fiat money has been sent. Afterward, you should verify if it has arrived. If the buyer does not respond, you can initiate a cancellation or a dispute. Remember, an administrator will NEVER contact you to resolve your order unless you open a dispute first.

- **fiat-sent-ok:**  
  - _To the buyer_:  I have informed to `seller-npub` that you have sent the fiat money. When the seller confirms they have received your fiat money, they should release the funds. If they refuse, you can open a dispute.
  - _To the seller_: `buyer-npub` has informed that they have sent you the fiat money. Once you confirm receipt, please release the funds. After releasing, the money will go to the buyer and there will be no turning back, so only proceed if you are sure. If you want to release the Sats to the buyer, send me `release-order-message`.
  
- **released:** `seller-npub` has already released the Sats! Expect your invoice to be paid any time. Remember your wallet needs to be online to receive through the Lightning Network.

- **purchase-completed:** Your satoshis purchase has been completed successfully. I have paid your invoice, enjoy sound money!

- **hold-invoice-payment-settled:** Your Sats sale has been completed after confirming the payment from `buyer-npub`.

- **rate:** Please qualify your counterparty 

- **rate-received:** Rating successfully saved!

- **cooperative-cancel-initiated-by-you:** You have initiated the cancellation of the order ID: `id`. Your counterparty must agree to the cancellation too. If they do not respond, you can open a dispute. Note that no administrator will contact you regarding this cancellation unless you open a dispute first.

- **cooperative-cancel-initiated-by-peer:** Your counterparty wants to cancel order ID: `id`. Note that no administrator will contact you regarding this cancellation unless you open a dispute first. If you agree on such cancellation, please send me `cancel-order-message`.

- **cooperative-cancel-accepted:** Order `id` has been successfully cancelled!

- **dispute-initiated-by-you:** You have initiated a dispute for order Id: `id`.  A solver will be assigned to your dispute soon. Once assigned, I will share their npub with you, and only they will be able to assist you. You may contact the solver directly, but if they reach out first, please ask them to provide the token for your dispute. Your dispute token is: `user-token`.

- **dispute-initiated-by-peer:** Your counterparty has initiated a dispute for order Id: `${orderId}.` A solver will be assigned to your dispute soon. Once assigned, I will share their npub with you, and only they will be able to assist you. You may contact the solver directly, but if they reach out first, please ask them to provide the token for your dispute. Your dispute token is: `user-token`.

- **admin-took-dispute:** 
  - _To the admin_: Here are the details of the dispute order you have taken: `details`. You need to determine which user is correct and decide whether to cancel or complete the order. Please note that your decision will be final and cannot be reversed.
  - _To the users_: The solver `admin-npub` will handle your dispute.  You can contact them directly, but if they reach out to you first, make sure to ask them for your dispute token.

- **admin-canceled:**
  - _To the admin_: You have cancelled the order ID: `id`!
  - _To the users_: Admin has cancelled the order ID: `id`!

- **admin-settled:** 
  - _To the admin_: You have completed the order ID: `id`!
  - _To the users_: Admin has completed the order ID: `id`!
  
- **is-not-your-dispute:** This dispute was not assigned to you! 

- **not-found:** Dispute not found. 

- **payment-failed:** I tried to send you the Sats but the payment of your invoice failed, I will try `payment_attempts` more times in `payment_retries_interval` minutes window. Please ensure your node/wallet is online.

- **invoice-updated:** Invoice has been successfully updated!
  
- **hold-invoice-payment-canceled:** The invoice was cancelled, your Sats will be available at your wallet again.

- **cant-do:** You are not allowed to `action` for this order!

- **admin-add-solver:** You have successfully added to the solver `npub`.

- **is-not-your-order:** You did not create this order and are not authorized to `action` it.
 
- **not-allowed-by-status:** You are not allowed to `action` because order Id `id` status is `order-status`.  

- **out-of-range-fiat-amount:** The requested amount is incorrect and may be outside the acceptable range. The minimum is  `min_amount` and the maximum is `max_amount`.   

- **incorrect-invoice-amount:** 
  - _If the buyer previously had sent the `new-order` action_:     
An invoice with non-zero amount was receive for the new order. Please send an invoice with a zero amount or no invoice at all.
  - _If the buyer previously sent the `add-invoice` action_:    
  The amount stated in the invoice is incorrect. Please send an invoice with an amount of `amount` satoshis, an invoice without an amount, or a lightning address.

- **invalid-sats-amount:** That specified Sats amount is invalid.

- **out-of-range-sats-amount:** The allowed Sats amount for this Mostro is between min `min_order_amount` and max `max_order_amount`. Please enter an amount within this range.
