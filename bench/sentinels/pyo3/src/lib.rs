use pyo3::prelude::*;

#[pyfunction]
fn greet() -> &'static str {
    "greeting of the first commit"
}

#[pymodule]
fn sentinel_pyo3(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(greet, module)?)
}
