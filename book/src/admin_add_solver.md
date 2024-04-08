# Admin add solver

Solvers are users appointed by the Mostro administrator and are responsible for resolving disputes.

The administrator can add or remove them at any time.

The administrator can also solve disputes.

An admin can add a solver, for this the admin will need to send an `order` message to Mostro with action `admin-add-solver`:

```json
{
  "order": {
    "version": 1,
    "pubkey": null,
    "action": "admin-add-solver",
    "content": {
      "text_message": "npub1qqq884wtp2jn96lqhqlnarl4kk3rmvrc9z2nmrvqujx3m4l2ea5qd5d0fq"
    }
  }
}
```

## Mostro response

Mostro will send this message to the admin:

```json
{
  "order": {
    "version": 1,
    "pubkey": null,
    "action": "admin-add-solver",
    "content": null
  }
}
```
