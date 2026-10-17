//! Isolation for app tests that mutate the process-global locale or assert
//! localized output. Hold this guard for the whole fixture, including window
//! hydration and executor work, rather than locking individual setter calls.

use parking_lot::{Mutex, MutexGuard};

use crate::i18n::AppLanguage;

static LOCALE: Mutex<()> = Mutex::new(());

pub(crate) struct TestLocale {
    previous: String,
    _lock: MutexGuard<'static, ()>,
}

impl TestLocale {
    pub(crate) fn new(language: AppLanguage) -> Self {
        Self::with_lock(LOCALE.lock(), language)
    }

    fn with_lock(lock: MutexGuard<'static, ()>, language: AppLanguage) -> Self {
        let guard = Self {
            previous: rust_i18n::locale().to_string(),
            _lock: lock,
        };
        crate::i18n::set_language(language);
        guard
    }
}

impl Drop for TestLocale {
    fn drop(&mut self) {
        // Restore the exact locale while still holding the lock. parking_lot
        // does not poison it when a test assertion unwinds.
        rust_i18n::set_locale(&self.previous);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    #[test]
    fn restores_the_previous_locale_after_success_and_panic() {
        let _locale = TestLocale::new(AppLanguage::Japanese);
        // The outer guard isolates this regression test from the app suite;
        // a separate lock lets us exercise nested fixture teardown directly.
        static FIXTURE: Mutex<()> = Mutex::new(());

        {
            let _fixture = TestLocale::with_lock(FIXTURE.lock(), AppLanguage::English);
            assert_eq!(crate::i18n::translate("menu.file"), "File");
        }
        assert_eq!(&*rust_i18n::locale(), "ja");
        assert_eq!(crate::i18n::translate("menu.file"), "ファイル");

        let result = catch_unwind(AssertUnwindSafe(|| {
            let _fixture = TestLocale::with_lock(FIXTURE.lock(), AppLanguage::English);
            panic!("simulate a failed fixture assertion");
        }));
        assert!(result.is_err());
        assert_eq!(&*rust_i18n::locale(), "ja");
        assert_eq!(crate::i18n::translate("menu.file"), "ファイル");

        let _fixture = TestLocale::with_lock(FIXTURE.lock(), AppLanguage::English);
        assert_eq!(crate::i18n::translate("menu.file"), "File");
    }

    #[test]
    fn another_fixture_cannot_acquire_the_locale_until_teardown() {
        let locale = TestLocale::new(AppLanguage::Japanese);
        assert!(
            std::thread::spawn(|| LOCALE.try_lock().is_none())
                .join()
                .unwrap(),
            "a parallel fixture must wait for the current one to finish"
        );
        assert_eq!(crate::i18n::translate("menu.file"), "ファイル");
        drop(locale);

        std::thread::spawn(|| {
            let _locale = TestLocale::new(AppLanguage::English);
            assert_eq!(crate::i18n::translate("menu.file"), "File");
        })
        .join()
        .unwrap();
    }
}
