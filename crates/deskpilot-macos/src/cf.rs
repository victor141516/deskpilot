//! Small CoreFoundation conversions shared by the modules.

use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, Type,
};

pub fn cfstr(s: &str) -> CFRetained<CFString> {
    CFString::from_str(s)
}

pub fn string(v: &CFType) -> Option<String> {
    v.downcast_ref::<CFString>().map(|s| s.to_string())
}

pub fn number(v: &CFType) -> Option<f64> {
    if let Some(b) = v.downcast_ref::<CFBoolean>() {
        return Some(if b.as_bool() { 1.0 } else { 0.0 });
    }
    let n = v.downcast_ref::<CFNumber>()?;
    n.as_f64().or_else(|| n.as_i64().map(|i| i as f64))
}

pub fn boolean(v: &CFType) -> Option<bool> {
    if let Some(b) = v.downcast_ref::<CFBoolean>() {
        return Some(b.as_bool());
    }
    number(v).map(|n| n != 0.0)
}

/// Elements of a CF array, as untyped CF objects.
pub fn array(v: &CFType) -> Vec<CFRetained<CFType>> {
    match v.downcast_ref::<CFArray>() {
        // SAFETY: CF arrays hold CF objects.
        Some(a) => unsafe { CFRetained::cast_unchecked::<CFArray<CFType>>(a.retain()) }.to_vec(),
        None => Vec::new(),
    }
}

/// Value for a string key in an untyped CF dictionary.
pub fn dict_get(dict: &CFDictionary, key: &str) -> Option<CFRetained<CFType>> {
    // SAFETY: the dictionaries we read (CGWindowList, AX) have CFString keys
    // and CF object values.
    let typed = unsafe { &*(dict as *const CFDictionary).cast::<CFDictionary<CFString, CFType>>() };
    typed.get(&cfstr(key))
}

