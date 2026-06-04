fn main() {
    let mut x = [0u8; 4];
    let mut y = [0u8; 4];
    let bufs: &mut [&mut [u8]] = &mut [&mut x[..], &mut y[..]];
    let slices: Vec<&mut [u8]> = bufs.iter_mut().map(|b| &mut **b).collect();
    for buf in &slices {
        let _ = (*buf).as_mut_ptr();
    }
}
