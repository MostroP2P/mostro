# Branch Protection Rules - MostroP2P

## 🛡️ Protected Repositories

The following `main` branches are now protected:

- ✅ **mostro** (main daemon)
- ✅ **mostro-core** (core library)
- ✅ **app** (Flutter mobile client)
- ✅ **mobile** (alternative mobile client)
- ✅ **mostro-cli** (CLI tool)
- ✅ **protocol** (protocol documentation)

---

## 🚫 Active Restrictions

### You CANNOT:

1. **❌ Push directly to `main`**
   ```bash
   git push origin main
   # Error: Push to protected branch rejected
   ```

2. **❌ Force push**
   ```bash
   git push --force origin main
   # Error: Force push disabled
   ```

3. **❌ Delete the `main` branch**
   ```bash
   git push origin --delete main
   # Error: Branch deletion disabled
   ```

---

## ✅ Correct Workflow

### 1. Create a feature branch

```bash
git checkout -b feature/my-feature
# Make changes
git add .
git commit -m "feat: description of change"
git push origin feature/my-feature
```

### 2. Create Pull Request

```bash
# On GitHub:
# 1. Go to https://github.com/MostroP2P/<repo>/pulls
# 2. Click "New Pull Request"
# 3. Select: base: main <- compare: feature/my-feature
# 4. Add clear description
# 5. Click "Create Pull Request"
```

### 3. Code Review

- ✅ **Minimum 1 approval required** before merge
- ✅ **All comments must be resolved**
- ✅ If new commits are pushed, previous approvals are dismissed (stale reviews)

### 4. Merge

```bash
# Once approved:
# - Click "Merge Pull Request" on GitHub
# - Confirm merge
# - Optional: Delete branch after merge
```

---

## 🚨 What if I already pushed directly to main?

### If you just pushed (and nobody has pulled):

**Admins can revert:**

```bash
# 1. Identify the bad commit
git log --oneline -5

# 2. Revert (creates new commit that undoes the changes)
git revert <commit-hash>
git push origin main

# Or if there are multiple bad commits:
git revert <commit-hash-1>..<commit-hash-n>
git push origin main
```

**Note:** Admins can push directly in emergencies, but should use this carefully.

### If others have already pulled:

**DO NOT use `git reset --hard` + force push** — it breaks everyone else's history.

**Correct option:**
1. Create PR with fix/revert
2. Merge normally

---

## 🔥 Emergencies (Admins Only)

**Admins can bypass** protections in critical cases:

**When it's acceptable:**
- 🔥 Critical security hotfix
- 🔥 Bug fix that blocks production
- 🔥 Revert accidental commit (if nobody else has pulled)

**Process:**
1. Notify in the work group
2. Explain why it's an emergency
3. Make the direct change
4. Document in CHANGELOG or commit message

---

## 📋 Rules Summary

| Action | Allowed | Requires |
|--------|---------|----------|
| Direct push to `main` | ❌ | - |
| Force push to `main` | ❌ | - |
| Create PR | ✅ | Nothing |
| Merge PR | ✅ | 1 approval + resolved comments |
| Delete `main` branch | ❌ | - |
| Admin bypass | ⚠️ | Judgment (emergencies only) |

---

## 🎯 Benefits

- ✅ **Prevents accidents** (like the direct push to main that motivated this)
- ✅ **Mandatory code review** (improves code quality)
- ✅ **Clean history** (no force pushes that break git)
- ✅ **Comments must be resolved** (discussions don't get left hanging)
- ✅ **Protects production** (main is always in a deployable state)

---

## ❓ FAQ

### What if I need to make an urgent change?

**Option 1 (preferred):** Quick PR + request urgent review in the group.

**Option 2 (real emergency):** Admin pushes directly + notifies the team.

### Can I approve my own PR?

Yes, but it **requires at least 1 approval from another person** before merge.

### What happens if I accidentally push to main?

GitHub rejects it:
```
! [remote rejected] main -> main (protected branch hook declined)
```

Simply create a PR with those changes.

### Are CI tests mandatory?

Not currently. Can be enabled later by adding `required_status_checks`.

---

## 📚 Resources

- [GitHub Branch Protection Docs](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-protected-branches/about-protected-branches)
- [Pull Request Best Practices](https://docs.github.com/en/pull-requests/collaborating-with-pull-requests)

---

**Updated:** 2026-03-26  
**By:** Mostronator (automation)  
**Reason:** Prevent accidental pushes to main
