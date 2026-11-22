//! The app macOS hands a URL to: Launch Services' default handler for its scheme, asked by the daemon (outside the
//! sandbox) before an `open` of the URL is offered.

use std::path::PathBuf;

/// The default handler of `url`'s scheme: the app bundle's path, or `None` when no app handles it (or the URL doesn't
/// parse).
pub fn default_handler(url: &str) -> Option<PathBuf> {
    ffi::default_application(url)
}

#[cfg(test)]
mod tests {
    /// Launch Services answers for a scheme every Mac handles, and for none.
    #[test]
    fn the_default_handler_is_an_app() {
        let mail = super::default_handler("mailto:a@example.com");
        assert!(mail.is_some_and(|p| p.extension().is_some_and(|e| e == "app")));
        assert_eq!(super::default_handler("piton-no-such-scheme-249:x"), None);
        assert_eq!(super::default_handler("not a url"), None);
    }
}

#[allow(unsafe_code)]
mod ffi {
    use std::{ffi::c_void, os::unix::ffi::OsStrExt, path::PathBuf};

    type CFTypeRef = *const c_void;
    type CFAllocatorRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFURLRef = *const c_void;
    type CFErrorRef = *mut c_void;
    type CFIndex = isize;
    type Boolean = u8;

    const UTF8: u32 = 0x0800_0100;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithBytes(
            alloc: CFAllocatorRef,
            bytes: *const u8,
            len: CFIndex,
            encoding: u32,
            external: Boolean,
        ) -> CFStringRef;
        fn CFURLCreateWithString(alloc: CFAllocatorRef, string: CFStringRef, base: CFURLRef) -> CFURLRef;
        fn CFURLGetFileSystemRepresentation(url: CFURLRef, resolve: Boolean, buffer: *mut u8, len: CFIndex) -> Boolean;
        fn CFRelease(cf: CFTypeRef);
    }

    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn LSCopyDefaultApplicationURLForURL(url: CFURLRef, role: u32, error: *mut CFErrorRef) -> CFURLRef;
    }

    /// `kLSRolesAll`.
    const ROLES_ALL: u32 = 0xFFFF_FFFF;

    /// Releases a Core Foundation object when dropped.
    struct Owned(CFTypeRef);

    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: the pointer came from a Create or Copy call and is released once.
                unsafe { CFRelease(self.0) }
            }
        }
    }

    pub fn default_application(url: &str) -> Option<PathBuf> {
        // SAFETY: each call gets valid pointers it only reads; every object created is released by `Owned`.
        unsafe {
            let string = Owned(CFStringCreateWithBytes(
                std::ptr::null(),
                url.as_ptr(),
                url.len() as CFIndex,
                UTF8,
                0,
            ));
            if string.0.is_null() {
                return None;
            }
            let url = Owned(CFURLCreateWithString(std::ptr::null(), string.0, std::ptr::null()));
            if url.0.is_null() {
                return None;
            }
            let mut error: CFErrorRef = std::ptr::null_mut();
            let app = Owned(LSCopyDefaultApplicationURLForURL(url.0, ROLES_ALL, &mut error));
            if !error.is_null() {
                CFRelease(error as CFTypeRef);
            }
            if app.0.is_null() {
                return None;
            }
            let mut buffer = vec![0u8; libc::PATH_MAX as usize];
            if CFURLGetFileSystemRepresentation(app.0, 1, buffer.as_mut_ptr(), buffer.len() as CFIndex) == 0 {
                return None;
            }
            let end = buffer.iter().position(|b| *b == 0)?;
            Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buffer[..end])))
        }
    }
}
