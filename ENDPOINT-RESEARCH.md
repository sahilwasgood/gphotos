# Endpoint research (honest findings)

This app talks to Google Photos using the browser's signed-in session and the
same `batchexecute` protocol the web app uses. To find out whether richer
endpoints exist, I statically analysed two real APKs and drove an authenticated
browser session. Findings below are evidence-backed; nothing is faked.

## What the desktop app can use (verified live)

These were captured from a signed-in Chrome session on `photos.google.com` and
replayed successfully through this app's own session:

| RPC | Returns | Used in app |
|---|---|---|
| `ZSBAec` | Things & Places groups (birthday, wedding, Mumbai, ...) with cover keys | Yes — Things & places view |
| `O3G8Nd` | A person profile (name, photo, gender) for a person id | No — needs a person id, no list RPC found |
| `yQelMe` | Keyed lookup, returned empty on replay | No |
| `Z5xsfc`, `snAcKc`, `EzkLib`, `EWgK9e` etc. | Albums, album items, timeline, storage | Yes — existing library code |

## What the mobile APKs reveal (static, not usable from browser cookies)

Two APKs were analysed: the official Google Photos 8.2.0 and the morphed
`app.morphe.android.apps.photos` 7.96.0. Both contain the same 250+ confirmed
`/ $rpc/<service>/<method>` paths on `photosdata-pa.googleapis.com`, including:

- **People / face groups**: `PhotosClusteringDataService/{PhotosReadClustersById,
  PhotosReadClusterItems, PhotosReadHiddenPersonClusters, PhotosWebReadUserClusters,
  PhotosSetMeCluster, ...}` and person-confirmation RPCs.
- **Sharing**: `PhotosSharingDataService/{PhotosUpdateLinkSharingState,
  PhotosGetSharedCollectionPersonClusters, PhotosUpdateCollabCamSettings, ...}`.
- **Quota**: `PhotosStorageQuotaDataService/{PhotosReadStorageQuota,
  PhotosListStoragePlans, PhotosGetSmartCleanupSummary, ...}`.
- **Search**: `PhotosSearchTabDataService/{PhotosStreamSearchItems,
  PhotosGetSearchSuggestions, ...}`.
- **Upload**: `photos.googleapis.com/data/upload/uploadmedia/{interactive,background}`.

These calls use OAuth2 bearer tokens (scopes `photos.native`, `mobileapps.native`),
not browser cookies. Replaying one with the desktop app's cookies returned HTTP
404, so they cannot be used here without shipping OAuth client credentials,
which would change the app's security model. `ListAccounts` does not appear in
either APK.

Research artifacts (evidence ids, full RPC lists, smali line references) are
kept outside the repository at `/tmp/opencode/rae-photos/`.

## Conclusion

The desktop client uses the richest endpoint set reachable with browser cookies.
People face-groups and sharing access/collaborators remain unavailable without
OAuth; the app opens the official web page for those actions rather than
pretending to support them.
