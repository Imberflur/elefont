//! A library that handles caching rendered glyphs on the GPU
//!
//! This fits as a layer in your rendering pipeline between font rasterization and shaping and text
//! rendering. In other words, first you turn a string into a series of font glyphs. Each of those
//! glyphs is looked up against the cache, and if it hasn't been rendered, it is turned into a
//! bitmap and uploaded to the GPU. The string is then laid out and rendered by the client
//! application.
//!
//! Scope of this library:
//! - DO support various font libraries / types of fonts (TTFs, bitmap fonts)
//! - DO support whatever backend (rendering to an image, GPU frameworks, etc.)
//! - DON'T handle complex tasks like shaping. The font stack should handle that elsewhere, and
//! provide this library the glyphs to render
//! - DON'T handle layout. This can be taken care of by the client
//! application when rendering.
//!
//! Support is available out-of-the-box for software rendering via `image`, rendering via
//! `rusttype` or `fontdue`, and performing automatic unicode normalization. All of these are optional features.

#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;

#[cfg(feature = "fontdue")]
pub mod fontdue_provider;
#[cfg(feature = "image")]
mod image_impl;
#[cfg(feature = "rusttype")]
pub mod rusttype_provider;

#[cfg(not(feature = "unicode-normalization"))]
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
#[cfg(feature = "unicode-normalization")]
use alloc::string::String;
use alloc::vec::Vec;
use hashbrown::HashMap;

/// Any object that can turn characters into glyphs and render them can be a FontProvider
///
/// FontProviders can be TTF font rasters, like rusttype (a pure-Rust library for decoding fonts) or
/// fontkit (a library that delegates to system APIs to handle fonts). Other FontProviders could
/// include bitmap fonts, or a combination of libraries (like a library to handle shaping and
/// another library to handle rendering.)
///
/// It is assumed that a given font provider will operate at a fixed size. For a variable-sized
/// source (like a TTF font), the font size can be paired with the font data to produce a single
/// FontProvider.
pub trait FontProvider {
    /// The format of the data generated by the FontProvider
    fn pixel_type(&self) -> PixelType;
    /// Convert a single character into a Glyph
    ///
    /// Generally you should use [`glyphs`], but when rendering just one character this method can
    /// be useful
    fn single_glyph(&self, character: char) -> Glyph;
    /// Convert the string into glyphs, and push the glyphs into the provided buffer
    ///
    /// This is not necessarily the same as running `single_glyph` over every character in the
    /// string! Text is hard.
    fn glyphs(&self, string: &str, glyphs: &mut Vec<Glyph>);
    /// How much space to include between baselines of the given font
    fn line_height(&self) -> f32;
    /// Get the metrics of a character (how to space it, where to include it on a line, etc.)
    fn metrics(&self, glyph: Glyph) -> Metrics;
    /// Convert a character into image bytes, with the format determined by [`pixel_type`]
    ///
    /// [`pixel_type`]: FontProvider::pixel_type
    fn rasterize(&self, glpyh: Glyph) -> Result<Vec<u8>, CacheError>;
    /// Optionally expose extra kerning information for glyphs
    ///
    /// By default, this is always 0.0. Some font providers may add more information here,
    /// however.
    fn kerning(&self, _a: Glyph, _b: Glyph) -> f32 {
        0.0
    }
}

/// Any object that can take the data for glyphs and store it over time
///
/// Textures can be image buffers on the CPU (like ones provided by the image crate) or a buffer
/// on the GPU, through any graphics library.
pub trait Texture {
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    /// Write the data from a font into a texture
    fn put_rect(&mut self, pixel: PixelType, data: &[u8], gpu: &TextureGlyph);
}

/// The main structure for maintaing a cache of rendered glyphs
///
/// `FontCache` is specifically an intermediary step. It doesn't understand how to read font files
/// or how to break up a string into glyphs: that's handled by the [`FontProvider`]. It doesn't
/// handle sending glyphs to the GPU: if you want to do that, provide a [`Texture`] that stores its
/// data on the GPU. What it does do is keep track of which glyphs have already been rendered, where
/// they were stored, and provide a consistent API over a variety of ways of rendering characters.
pub struct FontCache<T: Texture> {
    glyph_buffer: Vec<Glyph>,
    cache: Cache<T>,
}

struct Cache<T: Texture> {
    font: Box<dyn FontProvider>,
    texture: T,
    map: HashMap<Glyph, TextureGlyph>,
    h_cursor: u32,
    v_cursor: u32,
    current_line_height: u32,
}

impl<T: Texture> FontCache<T> {
    /// Create a new FontCache that pulls from the given provider and renders to the provided
    /// texture
    pub fn new(font: Box<dyn FontProvider>, texture: T) -> Self {
        FontCache {
            glyph_buffer: Vec::new(),
            cache: Cache {
                font,
                texture,
                map: HashMap::new(),
                h_cursor: 0,
                v_cursor: 0,
                current_line_height: 0,
            },
        }
    }

    /// Forget the position of the characters in the texture, and re-set the cursor.
    ///
    /// This doesn't set any data in the Texture! Old glyphs may continue to work, but this is akin
    /// to a use-after-free.
    pub fn clear(&mut self) {
        self.cache.clear();
    }

    /// Render a glyph to the texture
    pub fn render_glyph(&mut self, key: Glyph) -> Result<(Metrics, TextureGlyph), CacheError> {
        self.cache.render_glyph(key)
    }

    /// Attempt to convert a string into a series of glyphs or errors
    ///
    /// Before being converted, the string is normalized if the "unicode-normalilzation" feature is
    /// activated, and whitespace characters are removed.
    pub fn render_string<'a>(
        &'a mut self,
        string: &str,
    ) -> impl 'a + Iterator<Item = Result<(Metrics, TextureGlyph), CacheError>> {
        #[cfg(feature = "unicode-normalization")]
        let mut string = {
            use unicode_normalization::UnicodeNormalization;
            string.nfc().collect::<String>()
        };
        #[cfg(not(feature = "unicode-normalization"))]
        let mut string = string.to_owned();
        string.retain(|c| !c.is_whitespace());
        let glyph_buffer = &mut self.glyph_buffer;
        let cache = &mut self.cache;
        cache.font.glyphs(&string, glyph_buffer);
        glyph_buffer
            .drain(..)
            .map(move |glyph| cache.render_glyph(glyph))
    }

    /// Cache a string or return an error if one occurred
    ///
    /// This can be useful if the entire domain of the possible glyphs is known beforehand (like a
    /// bitmap font.) Under the hood, this just calls [`render_string`] and ignores the returned
    /// glyphs.
    pub fn cache_string(&mut self, string: &str) -> Result<(), CacheError> {
        self.render_string(string).map(|r| r.map(|_| ())).collect()
    }

    /// Swap out the internal texture for another one
    ///
    /// This will clear the cache automatically, to avoid holding references to invalid areas of
    /// the texture
    pub fn replace_texture(&mut self, mut texture: T) -> T {
        self.clear();
        core::mem::swap(&mut self.cache.texture, &mut texture);

        texture
    }

    pub fn texture(&self) -> &T {
        &self.cache.texture
    }

    pub fn font(&self) -> &dyn FontProvider {
        self.cache.font.as_ref()
    }
}

impl<T: Texture> Cache<T> {
    fn clear(&mut self) {
        self.map.clear();
        self.h_cursor = 0;
        self.v_cursor = 0;
        self.current_line_height = 0;
    }

    fn render_glyph(&mut self, glyph: Glyph) -> Result<(Metrics, TextureGlyph), CacheError> {
        if let Some(tex_glyph) = self.map.get(&glyph) {
            return Ok((self.font.metrics(glyph), *tex_glyph));
        }
        let metrics = self.font.metrics(glyph);
        let bounds = metrics.bounds.unwrap();
        if bounds.width > self.texture.width() || bounds.height > self.texture.height() {
            return Err(CacheError::TextureTooSmall);
        }
        if bounds.width + self.h_cursor > self.texture.width() {
            self.h_cursor = 0;
            self.v_cursor += self.current_line_height + 1;
            self.current_line_height = 0;
        }
        if bounds.height + self.v_cursor > self.texture.height() {
            return Err(CacheError::OutOfSpace);
        }
        let pixel_type = self.font.pixel_type();
        let data = self.font.rasterize(glyph)?;
        let gpu = TextureGlyph {
            glyph,
            bounds: Bounds {
                x: self.h_cursor as i32,
                y: self.v_cursor as i32,
                width: bounds.width,
                height: bounds.height,
            },
        };
        self.texture.put_rect(pixel_type, &data[..], &gpu);
        self.h_cursor += gpu.bounds.width + 1;
        self.current_line_height = self.current_line_height.max(gpu.bounds.height);
        self.map.insert(glyph, gpu);

        Ok((self.font.metrics(glyph), gpu))
    }
}

/// The index of the font character to render
///
/// Glyphs are what actually gets rendered to the screen. It might be tempting to think of a glyph
/// like a 'rendered character.' In specific scripts, this is often the case. 'A' and 'a' have
/// distinct glyphs, and are unconditionally the same glyph. In others, this might not be true. See
/// ['Text Rendering Hates You'](https://gankra.github.io/blah/text-hates-you) for more information
/// on why text is complicated.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Glyph(pub u32);

/// The relevant information for a glyph stored on the texture
#[derive(Copy, Clone, Debug)]
pub struct TextureGlyph {
    pub glyph: Glyph,
    pub bounds: Bounds,
}

/// The layout information for a glyph
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct Metrics {
    pub bounds: Option<Bounds>,
    pub bearing_x: f32,
    pub advance_x: f32,
    pub bearing_y: f32,
    pub advance_y: f32,
}

#[derive(Copy, Clone, Debug)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// An error generated during a cache operation
#[derive(Copy, Clone, Debug)]
pub enum CacheError {
    /// No matter what, the texture is too small to render the glyph (even when empty)
    ///
    /// To fix this error, expand the texture. Make sure to clear the cache if the texture data is
    /// also invalidated
    TextureTooSmall,
    /// The cache cannot store the current request without clearing it first
    OutOfSpace,
    /// A glyph was passed to a render method but it could not be rendered
    ///
    /// For example, unsized glyphs (glyphs with None for their [`bounds`]) cannot be rendered
    ///
    /// [`bounds`]: Metrics::bounds
    NonRenderableGlyph(Glyph),
}

#[cfg(feature = "std")]
use std::{error::Error, fmt};

#[cfg(feature = "std")]
impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CacheError::TextureTooSmall => {
                write!(f, "The texture is too small to render the given input")
            }
            CacheError::OutOfSpace => write!(
                f,
                "The cache is out of space, and must be cleared before more rendering"
            ),
            CacheError::NonRenderableGlyph(glyph) => {
                write!(f, "Attempted to render an un-renderable glyph: {:?}", glyph)
            }
        }
    }
}

#[cfg(feature = "std")]
impl Error for CacheError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        None
    }
}

/// How the pixels of the rasterized font are represented
pub enum PixelType {
    /// A series of values representing the alpha with no associated color
    Alpha,
    /// A series of complete color values
    RGBA,
}
