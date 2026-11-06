# Migration vers l'org GitHub `weebo-si-operator`

| Champ  | Valeur |
| ------ | ------ |
| Statut | Préparée, rien n'est transféré |
| Créée  | 2026-11-06 |
| Cible  | <https://github.com/weebo-si-operator> (plan free, vide au 2026-11-06) |

## 1. Périmètre

**Migrent** (le kit et tous les opérateurs bâtis dessus) :

| Dépôt | Kit épinglé | Image publiée aujourd'hui | Chart OCI |
| --- | --- | --- | --- |
| `weebo-operator-kit` | — | — (chart `weebo-operator-lib`, tarball `@weebo/operator-docs`) | `ghcr.io/batleforc/charts/weebo-operator-lib` |
| `weebo-forgejo` | v0.27.0 | `ghcr.io/batleforc/weebo-forgejo-operator` | `ghcr.io/batleforc/charts/weebo-forgejo` |
| `weebo-authentik` | v0.25.1 | `ghcr.io/batleforc/weebo-authentik-operator` | `ghcr.io/batleforc/charts/weebo-authentik` |
| `weebo-angos` | v0.24.0 | `ghcr.io/batleforc/weebo-angos-operator` | `ghcr.io/batleforc/charts/weebo-angos` |
| `weebo-batlehub` | v0.24.0 | `ghcr.io/batleforc/weebo-batlehub-operator` | `ghcr.io/batleforc/charts/weebo-batlehub` |
| `weebo-stalwart` | v0.24.0 | (pas de clone local : à inventorier) | `ghcr.io/batleforc/charts/weebo-stalwart` |

Les dépôts à venir (`weebo-openbao`, RFC 0007 ; `weebo-link`, RFC 0002) sont
créés directement dans l'org.

**Restent chez `batleforc`** : `ProxyAuthK8S`, `weebo-si-hardening`, et
`weebo-si` (le consommateur, qui change ses références, §5). Les références à
ces dépôts ne sont pas réécrites, en particulier les 59 liens de
weebo-authentik vers `batleforc/proxyauthK8s`.

## 2. Ce que GitHub redirige, et ce qu'il ne redirige pas

Un transfert de dépôt garde une redirection de l'ancien chemin vers le
nouveau, **tant qu'aucun dépôt n'est recréé sous l'ancien nom**. Ne jamais
recréer `batleforc/weebo-<x>` après le transfert.

| Référence | Exemple | Après transfert |
| --- | --- | --- |
| git (Cargo `git = …`, clone) | `github.com/batleforc/weebo-operator-kit` | redirigée |
| raw, épinglée par sha256 | `raw.githubusercontent.com/batleforc/weebo-operator-kit/v0.27.0/tasks/rust.yaml` | redirigée, contenu identique |
| workflow réutilisable | `uses: batleforc/weebo-operator-kit/.github/workflows/publish.yml@v0.27.0` | redirigée d'après GitHub : **à vérifier** par une CI (étape 2) avant de compter dessus |
| release asset | `…/releases/download/vX/weebo-operator-docs-X.tgz` | redirigée |
| **image et chart GHCR** | `ghcr.io/batleforc/charts/weebo-forgejo` | **non** : les paquets restent sous `batleforc`, les nouvelles versions partent sous `ghcr.io/weebo-si-operator/…` |
| **GitHub Pages** | `batleforc.github.io/weebo-forgejo` | **non** : le site devient `weebo-si-operator.github.io/weebo-forgejo` |
| **identité cosign keyless** | `https://github.com/batleforc/weebo-operator-kit/.github/workflows/release.yml@refs/tags/v…` | **change** : les signatures après transfert portent `weebo-si-operator` |

Les trois dernières lignes font l'ordre du plan. Le piège principal :
`publish.yml` du kit vérifie la signature du chart `weebo-operator-lib` avec
une identité **codée en dur** (`.github/workflows/publish.yml`, étape
« Verify the kit's library chart signature », regexp
`^https://github\.com/batleforc/weebo-operator-kit/…`). Un opérateur qui
monte sur un chart de kit signé après le transfert échoue à cette étape tant
que la regexp n'accepte pas le nouveau propriétaire.

## 3. Préparer l'org (avant tout transfert)

- [ ] Paramètres Actions de l'org : Actions autorisées, workflows
      réutilisables des dépôts publics de l'org autorisés, permissions par
      défaut du `GITHUB_TOKEN` en lecture (les jobs déclarent déjà
      `packages: write`, `id-token: write`, `pages: write` là où il faut).
- [ ] Paquets : création de paquets publics autorisée pour les membres.
- [ ] Pages : activées pour l'org. Décider d'un domaine personnalisé
      (`docs.weebo.fr/<op>` par exemple) **maintenant**, pour ne plus casser
      les URLs du site à un futur déplacement.
- [ ] Renovate (GitHub App) installé sur l'org, avec les dépôts migrés.
- [ ] 2FA exigée, et les membres ou équipes qui ont le droit de pousser des
      tags (les tags déclenchent les releases).
- [ ] Relever, par dépôt, ce que le transfert ne garde pas forcément :
      secrets et variables Actions, environments, règles de protection et
      rulesets, webhooks, deploy keys. `gh secret list`, `gh variable list`,
      `gh api repos/batleforc/<x>/rulesets` avant, comparés après.

## 4. Ordre de migration

Chaque étape laisse tout vert. Les anciens épinglages continuent de marcher
par les redirections, et les anciens paquets GHCR restent en place.

1. **Transférer le kit** : `gh api -X POST repos/batleforc/weebo-operator-kit/transfer -f new_owner=weebo-si-operator`
   (geste du propriétaire, irréversible sauf retransfert). Rien d'autre ne
   change ce jour-là.
2. **Vérifier les redirections** : relancer la CI de weebo-angos (kit v0.24.0,
   `uses: batleforc/…/checks.yml@v0.24.0`, Taskfile raw, Cargo git). Si le
   `uses:` ne suit pas la redirection, l'étape 3 devient bloquante pour
   chaque opérateur : il doit monter de version de kit avant sa prochaine CI.
3. **Release du kit** (minor, `feat`) qui réécrit ses propres références :
   - `Cargo.toml`, `templates/operator/**`, `templates/pin-kit.sh`
     (`repo=weebo-si-operator/weebo-operator-kit`), README, docs, site ;
   - `publish.yml` : `chart_repo` par défaut `ghcr.io/weebo-si-operator/charts`,
     exemples d'image en `ghcr.io/weebo-si-operator/…`, et la regexp
     d'identité cosign qui **accepte les deux propriétaires** le temps de la
     transition :
     `^https://github\.com/(batleforc|weebo-si-operator)/weebo-operator-kit/\.github/workflows/release\.yml@refs/tags/v`
   - le chart `weebo-operator-lib` publié sous
     `ghcr.io/weebo-si-operator/charts` ;
   - `packages/operator-docs` : URLs de dépôt et de site ;
   - `site` : `DOCS_BASE_PATH` inchangé, hôte nouveau.
   `task test:all` vert, dont `template:check` (le template rendu doit
   pointer sur la nouvelle org).
4. **Pour chaque opérateur**, dans l'ordre forgejo, authentik, angos,
   batlehub, stalwart. **Sans monter de version de kit** : les tags suivent
   le dépôt transféré, et `publish.yml` du kit a les entrées `image` et
   `chart-repo` depuis v0.24.0 au moins.
   1. transférer le dépôt ;
   2. réécrire ses références (§5, `MIGRATED=1`) : son propre dépôt, son
      image, son site, et le kit **au même tag** (`Cargo.toml`, URLs raw du
      Taskfile, dont le sha256 ne change pas, `uses: …@vX`, `KIT_REPO`),
      puis `cargo update --workspace --offline` ou un `cargo check` pour
      réécrire la source dans `Cargo.lock` ;
   3. **garder** `repository: oci://ghcr.io/batleforc/charts` sur la
      dépendance `weebo-operator-lib` du Chart.yaml : les versions de ce
      chart antérieures à la release de l'étape 3 n'existent que là, signées
      par l'ancienne identité, ce que le `publish.yml` épinglé vérifie ;
   4. `publish.yml` de l'opérateur : `image: ghcr.io/weebo-si-operator/<op>-operator`
      et `chart-repo: ghcr.io/weebo-si-operator/charts` passés explicitement
      (le défaut des anciens tags du kit vise `batleforc`, où un dépôt de
      l'org ne peut pas pousser) ;
   5. CI verte, puis release de l'opérateur : la première image et le premier
      chart sous `ghcr.io/weebo-si-operator/…`.
   La montée de kit (vers la release de l'étape 3 ou suivante) reste un
   travail à part, au rythme habituel : c'est elle qui bascule la dépendance
   `weebo-operator-lib` vers la nouvelle org.
5. **weebo-si**, opérateur par opérateur, après sa release de l'étape 4 :
   chart OCI et image dans les values ArgoCD, `renovate.json5` (les règles
   qui matchent `ghcr.io/batleforc/charts/…`, Renovate ne suit pas un
   changement de nom de paquet), liens de doc (`batleforc.github.io/<op>` →
   nouvel hôte), et toute vérification de signature qui fixerait l'identité.
6. **Nettoyage**, quand tous les consommateurs sont passés : une release du
   kit qui restreint la regexp cosign à `weebo-si-operator`. Les anciens
   paquets `ghcr.io/batleforc/…` **restent** (versions épinglées de
   l'historique, rollback d'ArgoCD) : ne pas les supprimer.

## 5. Réécriture des références

Une substitution bornée aux dépôts qui migrent, à lancer à la racine de chaque
dépôt (`MIGRATED=1` dans un dépôt qui migre, où tout `ghcr.io/batleforc/charts`
est le kit ou l'opérateur lui-même ; sans, dans weebo-si, où ce préfixe porte
aussi des charts qui restent), puis à relire au diff :

```bash
names='weebo-operator-kit|weebo-forgejo|weebo-authentik|weebo-angos|weebo-batlehub|weebo-stalwart'
if [ -n "${MIGRATED:-}" ]; then
  charts_re='ghcr\.io/batleforc/charts()\b'
else
  charts_re="ghcr\.io/batleforc/charts(/(${names}|weebo-operator-lib))\b"
fi
rg -l --hidden -g '!.git' -g '!target' -g '!node_modules' -g '!graphify-out' -g '!*.lock' \
   "\bbatleforc/(${names})\b|batleforc\.github\.io/(${names})\b|${charts_re}" \
 | xargs -r sed -i -E \
   -e "s#\bbatleforc/(${names})\b#weebo-si-operator/\1#g" \
   -e "s#batleforc\.github\.io/(${names})\b#weebo-si-operator.github.io/\1#g" \
   -e "s#${charts_re}#ghcr.io/weebo-si-operator/charts\1#g"
```

Dans weebo-si, réduire `names` aux opérateurs **déjà publiés** sous l'org
(§4.5) : la commande bascule sinon des charts qui n'existent pas encore.
Essai à blanc dans weebo-si : seuls les charts et liens migrés changent,
`ghcr.io/batleforc/charts/endpoint-gateway`, le chart `weebo-si-operator` de
`ghcr.io/batleforc/charts`, ProxyAuthK8S et weebo-si-hardening restent.

`\bbatleforc/<nom>\b` couvre d'un coup `github.com/…`, `raw.githubusercontent.com/…`,
`uses: …`, `KIT_REPO: …` (`.tasks/kit.yaml`) et les images
`ghcr.io/batleforc/<nom>-operator`. Essai à blanc sur des copies le
2026-11-06 : weebo-angos et weebo-authentik n'en gardent que des
`batleforc.fr`, des identités de test et les liens vers ProxyAuthK8S,
weebo-si et WeeboDevImage, qui restent.

À traiter à la main, la substitution ne les voit pas ou ne doit pas les voir :

- dans weebo-si, un `ghcr.io/batleforc/charts` sans nom de chart (la
  substitution ne le touche pas sans `MIGRATED=1`) ;
- dans un opérateur, `MIGRATED=1` réécrit aussi la dépendance
  `weebo-operator-lib` du Chart.yaml : la remettre sur `batleforc` tant que
  l'opérateur n'a pas monté son kit (§4.4.3) ;
- la regexp cosign de §4.3, qui doit garder les deux propriétaires un temps ;
- les sha256 des includes Taskfile : l'URL change, le contenu non, mais la
  nouvelle version de kit change le contenu ;
- dans **weebo-si**, les images et charts : à ne basculer qu'après la release
  de l'opérateur concerné (§4.5), sinon ArgoCD vise un paquet qui n'existe
  pas encore ;
- les références aux images qui ne sont pas des opérateurs
  (`ghcr.io/batleforc/batlehub`, `weebo-notify-v2`, `weebodevimage`…) : elles
  restent.

Inventaire au 2026-11-06 (fichiers contenant `batleforc`, hors lock et
`target`) : kit 51, weebo-forgejo 40, weebo-authentik 29, weebo-angos 24,
weebo-batlehub 26, weebo-si 58. weebo-stalwart à inventorier après clone.

## 6. Vérification

Par opérateur, après sa release :

- `gh run list -R weebo-si-operator/<op>` : CI et publish verts ;
- `cosign verify ghcr.io/weebo-si-operator/<op>-operator:<tag>
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
  --certificate-identity-regexp '^https://github\.com/weebo-si-operator/<op>/'` ;
- `helm pull oci://ghcr.io/weebo-si-operator/charts/<op> --version <tag>` ;
- le site sur le nouvel hôte ;
- `rg '\bbatleforc/(weebo-operator-kit|weebo-forgejo|…)\b'` vide dans le
  dépôt. Les liens de `CHANGELOG.md` sont réécrits aussi : ils pointeraient
  sinon sur une redirection, que recréer un dépôt casserait.

## 7. Retour arrière

Un dépôt se retransfère (`gh api -X POST repos/weebo-si-operator/<x>/transfer -f new_owner=batleforc`).
Les anciens paquets GHCR étant conservés, weebo-si peut revenir à
`ghcr.io/batleforc/…` en un commit. Les paquets publiés sous l'org y
restent.
