# XPub Onboarding

Connect a store's receiving wallet by pasting an extended public key (xpub)
into the dashboard. An xpub derives the addresses customers pay into. It cannot
spend.

<!-- TODO: once the dashboard accepts a derive-xpub JSON file upload, add that step to "Paste the xpub into the dashboard". -->

## Get an xpub

**Browser-extension wallets cannot export one.** They show a single address per
account and have no control that exports the account-level extended key. Do not
look for one there.

Generate a dedicated receiving wallet with the offline CLI instead, from the
`evm/` directory of this repository:

```
cargo run --bin derive-xpub -- generate
```

It prints a new 24-word mnemonic, the account path `m/44'/60'/0'`, the xpub,
and the first three receiving addresses. The tool makes no network connections.

- Write the mnemonic down offline. Never type it into a website.
- Use this wallet only for receiving payments. Do not import it into a browser
  extension or reuse it as a daily wallet.
- Paste the **xpub** into the dashboard. Never paste an `xprv` (a private key).
  The server refuses it.

For the full background, see
[Where do I get an xpub?](merchant-api-guide.md#where-do-i-get-an-xpub) in the
integration guide.

## Paste the xpub into the dashboard

1. Open **Wallets** in the sidebar, under **Configuration**.
2. Click **Add wallet**.
3. In **Extended public key (xpub)**, paste the xpub. The placeholder is
   `xpub...`. The **Add wallet** button stays disabled until the field has a value.
4. Optional: enter a **Name (optional)**.
5. Click **Add wallet** to submit.
6. Open the wallet. It has two tabs:
   - **General** shows the masked key, with an **Export** button for the full
     xpub, and a **Delete wallet** button.
   - **Addresses** shows how many addresses have been derived so far. It does
     not list them, so use the comparison below.

The form also offers **Use my recovery phrase as a receiving wallet**. Its own
text warns that this makes the account recovery phrase also disclose every
receiving address and the payment history. This guide does not cover that route.
Use the paste route above.

The dashboard always registers a pasted key as an Ethereum (`eip155`) wallet.
It cannot choose another chain family.

## Compare the addresses

Do this before you take a payment. The dashboard does not show addresses, so
compare against your own wallet.

1. Open the receiving wallet in your own software.
2. Compare its first three receiving addresses with the `[0]`, `[1]` and `[2]`
   lines printed by `derive-xpub`. Each line shows its path,
   `m/44'/60'/0'/0/<index>`.
3. If they match, the xpub is correct.

Open question: the dashboard does not display the verification addresses that
the server returns when a wallet is created. Whether they should be shown is not
decided here.

Compare these addresses to what your wallet shows; if they don't match, stop.
Do not take payments to this store and do not create invoices. Check the key you
pasted and the path your wallet uses.
