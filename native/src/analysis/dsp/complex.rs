//! フィルタ設計用の複素数（numpy の complex128 と同じ式: 割り算は Smith 法、平方根は glibc の csqrt）。
use std::ops::{Add, Mul, Neg, Sub};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct C {
    pub re: f64,
    pub im: f64,
}

impl C {
    pub const fn new(re: f64, im: f64) -> C {
        C { re, im }
    }
    pub const fn real(re: f64) -> C {
        C { re, im: 0.0 }
    }
    pub fn conj(self) -> C {
        C::new(self.re, -self.im)
    }
    /// np.abs（hypot）
    pub fn abs(self) -> f64 {
        self.re.hypot(self.im)
    }
    /// np.isreal
    pub fn is_real(self) -> bool {
        self.im == 0.0
    }
    /// numpy の複素数の割り算（loops の CDOUBLE_divide。Smith 法）
    pub fn div(self, b: C) -> C {
        let (in1r, in1i, in2r, in2i) = (self.re, self.im, b.re, b.im);
        let in2r_abs = in2r.abs();
        let in2i_abs = in2i.abs();
        if in2r_abs >= in2i_abs {
            if in2r_abs == 0.0 && in2i_abs == 0.0 {
                return C::new(in1r / in2r_abs, in1i / in2i_abs);
            }
            let rat = in2i / in2r;
            let scl = 1.0 / (in2r + in2i * rat);
            C::new((in1r + in1i * rat) * scl, (in1i - in1r * rat) * scl)
        } else {
            let rat = in2r / in2i;
            let scl = 1.0 / (in2i + in2r * rat);
            C::new((in1r * rat + in1i) * scl, (in1i * rat - in1r) * scl)
        }
    }
    /// glibc の csqrt（オーバーフロー対策の拡大縮小は、フィルタ設計の値の範囲では起きないので省く）
    pub fn sqrt(self) -> C {
        let (x, y) = (self.re, self.im);
        if y == 0.0 {
            if x < 0.0 {
                return C::new(0.0, (-x).sqrt().copysign(y));
            }
            return C::new(x.sqrt(), y);
        }
        if x == 0.0 {
            let r =
                if y.abs() >= 2.0 * f64::MIN_POSITIVE { (0.5 * y.abs()).sqrt() } else { 0.5 * (2.0 * y.abs()).sqrt() };
            return C::new(r, r.copysign(y));
        }
        let d = x.hypot(y);
        let (r, s);
        if x > 0.0 {
            r = (0.5 * (d + x)).sqrt();
            s = 0.5 * (y / r);
        } else {
            s = (0.5 * (d - x)).sqrt();
            r = (0.5 * (y / s)).abs();
        }
        C::new(r, s.copysign(y))
    }
}

impl Add for C {
    type Output = C;
    fn add(self, b: C) -> C {
        C::new(self.re + b.re, self.im + b.im)
    }
}

impl Sub for C {
    type Output = C;
    fn sub(self, b: C) -> C {
        C::new(self.re - b.re, self.im - b.im)
    }
}

impl Mul for C {
    type Output = C;
    fn mul(self, b: C) -> C {
        C::new(self.re * b.re - self.im * b.im, self.re * b.im + self.im * b.re)
    }
}

impl Neg for C {
    type Output = C;
    fn neg(self) -> C {
        C::new(-self.re, -self.im)
    }
}

/// 実数 × 複素数（numpy は実数を (a, 0) にして複素数の掛け算をする）
pub fn rmul(a: f64, b: C) -> C {
    C::real(a) * b
}
