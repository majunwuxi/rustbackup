//! Google Drive one-way backup client.
//!
//! The API is transport-configurable so protocol tests can use a loopback mock.
//! Production requests use Google's HTTPS endpoints and the restricted
//! `drive.file` OAuth scope.

mod drive;
mod oauth;

pub use drive::{
    ArchivedDriveVersion, DriveClient, DriveEndpoints, DriveError, DriveFile, DriveFolders,
    DriveHistoryCopy, UploadResult,
};
pub use oauth::{
    begin_authorization, AccountIdentity, AuthorizationCode, CredentialStore, OAuthClient,
    OAuthEndpoints, OAuthError, OAuthSession, PlatformCredentialStore, TokenResponse,
};
