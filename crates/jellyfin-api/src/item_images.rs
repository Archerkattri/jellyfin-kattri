//! Authenticated, bounded artwork retrieval for item backdrop images.

use std::time::Duration;

use reqwest::{Method, StatusCode, header};

use crate::{ApiError, JellyfinClient, Session};

const MAX_BACKDROP_IMAGE_RESPONSE_BYTES: usize = 5 * 1024 * 1024;

/// Failure while fetching an item's first backdrop image.
#[derive(Debug, thiserror::Error)]
pub enum BackdropImageError {
    /// The item has no backdrop at image index zero, or the response was empty.
    #[error("the item has no backdrop image at index zero")]
    NotFoundOrEmptyImage,
    /// Another request or response failure.
    #[error(transparent)]
    Api(#[from] ApiError),
}

impl JellyfinClient {
    /// Fetch backdrop image index zero as a JPEG, with a strict response-size cap.
    pub async fn get_backdrop_image(
        &self,
        session: &Session,
        item_id: &str,
    ) -> Result<Vec<u8>, BackdropImageError> {
        // A full-width hero needs more than a card, but an original backdrop can be
        // 4K or larger; 1920 wide keeps it sharp and inside the response cap.
        self.fetch_backdrop_image(session, item_id, "1920", "90", None)
            .await
    }

    /// Fetch a 480px-wide backdrop image for ambient background blur/tinting.
    pub async fn get_ambient_backdrop_image(
        &self,
        session: &Session,
        item_id: &str,
        tag: Option<&str>,
    ) -> Result<Vec<u8>, ApiError> {
        self.fetch_backdrop_image(session, item_id, "480", "75", tag)
            .await
            .map_err(|error| match error {
                BackdropImageError::NotFoundOrEmptyImage => {
                    ApiError::HttpStatus(StatusCode::NOT_FOUND)
                }
                BackdropImageError::Api(api_error) => api_error,
            })
    }

    async fn fetch_backdrop_image(
        &self,
        session: &Session,
        item_id: &str,
        max_width: &str,
        quality: &str,
        tag: Option<&str>,
    ) -> Result<Vec<u8>, BackdropImageError> {
        if item_id.is_empty() || item_id.len() > 128 || item_id.chars().any(char::is_control) {
            return Err(ApiError::InvalidResponse.into());
        }

        let url = self.endpoint(&["Items", item_id, "Images", "Backdrop"]);
        let mut query = vec![
            ("imageIndex", "0"),
            ("format", "jpg"),
            ("maxWidth", max_width),
            ("quality", quality),
        ];
        if let Some(tag) = tag.filter(|tag| !tag.is_empty()) {
            query.push(("tag", tag));
        }
        let request = self
            .http
            .request(Method::GET, url)
            .query(&query)
            .timeout(Duration::from_secs(15));
        let request = self
            .add_metadata(request, Some(&session.access_token))
            .header(header::ACCEPT, "image/jpeg");
        let mut response = request.send().await.map_err(crate::transport_error)?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            return Err(BackdropImageError::NotFoundOrEmptyImage);
        }
        if status == StatusCode::UNAUTHORIZED {
            return Err(ApiError::ReauthenticationRequired.into());
        }
        if status == StatusCode::FORBIDDEN {
            return Err(ApiError::Forbidden.into());
        }
        if !status.is_success() {
            return Err(ApiError::HttpStatus(status).into());
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_BACKDROP_IMAGE_RESPONSE_BYTES as u64)
        {
            return Err(ApiError::ImageTooLarge.into());
        }

        let mut body = Vec::with_capacity(
            response
                .content_length()
                .unwrap_or(0)
                .min(MAX_BACKDROP_IMAGE_RESPONSE_BYTES as u64) as usize,
        );
        while let Some(chunk) = response.chunk().await.map_err(crate::transport_error)? {
            if body.len().saturating_add(chunk.len()) > MAX_BACKDROP_IMAGE_RESPONSE_BYTES {
                return Err(ApiError::ImageTooLarge.into());
            }
            body.extend_from_slice(&chunk);
        }
        if body.is_empty() {
            return Err(BackdropImageError::NotFoundOrEmptyImage);
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::BackdropImageError;

    #[test]
    fn not_found_or_empty_image_has_a_distinct_typed_error() {
        let error = BackdropImageError::NotFoundOrEmptyImage;
        assert_eq!(
            error.to_string(),
            "the item has no backdrop image at index zero"
        );
        assert!(matches!(error, BackdropImageError::NotFoundOrEmptyImage));
    }
}
