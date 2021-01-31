// Copyright (c) 2021-present, Gregory Szorc
// All rights reserved.
//
// This software may be modified and distributed under the terms
// of the BSD license. See the LICENSE file for details.

use {
    crate::{compression_dict::ZstdCompressionDict, exceptions::ZstdError},
    pyo3::{
        buffer::PyBuffer,
        exceptions::{PyMemoryError, PyNotImplementedError, PyValueError},
        prelude::*,
        types::PyBytes,
    },
    std::{marker::PhantomData, sync::Arc},
};

pub struct DCtx<'a>(*mut zstd_sys::ZSTD_DCtx, PhantomData<&'a ()>);

impl<'a> Drop for DCtx<'a> {
    fn drop(&mut self) {
        unsafe {
            zstd_sys::ZSTD_freeDCtx(self.0);
        }
    }
}

unsafe impl<'a> Send for DCtx<'a> {}
unsafe impl<'a> Sync for DCtx<'a> {}

impl<'a> DCtx<'a> {
    fn new() -> Result<Self, &'static str> {
        let dctx = unsafe { zstd_sys::ZSTD_createDCtx() };
        if dctx.is_null() {
            return Err("could not allocate ZSTD_DCtx instance");
        }

        Ok(Self(dctx, PhantomData))
    }
}

#[pyclass]
struct ZstdDecompressor {
    dict_data: Option<Py<ZstdCompressionDict>>,
    max_window_size: usize,
    format: zstd_sys::ZSTD_format_e,
    dctx: Arc<DCtx<'static>>,
}

impl ZstdDecompressor {
    fn setup_dctx(&self, py: Python, load_dict: bool) -> PyResult<()> {
        unsafe {
            zstd_sys::ZSTD_DCtx_reset(
                self.dctx.0,
                zstd_sys::ZSTD_ResetDirective::ZSTD_reset_session_only,
            );
        }

        if self.max_window_size != 0 {
            let zresult =
                unsafe { zstd_sys::ZSTD_DCtx_setMaxWindowSize(self.dctx.0, self.max_window_size) };
            if unsafe { zstd_sys::ZDICT_isError(zresult) } != 0 {
                return Err(ZstdError::new_err(format!(
                    "unable to set max window size: {}",
                    zstd_safe::get_error_name(zresult)
                )));
            }
        }

        let zresult = unsafe { zstd_sys::ZSTD_DCtx_setFormat(self.dctx.0, self.format) };
        if unsafe { zstd_sys::ZDICT_isError(zresult) } != 0 {
            return Err(ZstdError::new_err(format!(
                "unable to set decoding format: {}",
                zstd_safe::get_error_name(zresult)
            )));
        }

        if let Some(dict_data) = &self.dict_data {
            if load_dict {
                dict_data.try_borrow_mut(py)?.load_into_dctx(self.dctx.0)?;
            }
        }

        Ok(())
    }
}

#[pymethods]
impl ZstdDecompressor {
    #[new]
    #[args(dict_data = "None", max_window_size = "0", format = "0")]
    fn new(
        dict_data: Option<Py<ZstdCompressionDict>>,
        max_window_size: usize,
        format: u32,
    ) -> PyResult<Self> {
        let format = if format == zstd_sys::ZSTD_format_e::ZSTD_f_zstd1 as _ {
            zstd_sys::ZSTD_format_e::ZSTD_f_zstd1
        } else if format == zstd_sys::ZSTD_format_e::ZSTD_f_zstd1_magicless as _ {
            zstd_sys::ZSTD_format_e::ZSTD_f_zstd1_magicless
        } else {
            return Err(PyValueError::new_err(format!("invalid format value")));
        };

        let dctx = Arc::new(DCtx::new().map_err(|_| PyMemoryError::new_err(()))?);

        Ok(Self {
            dict_data,
            max_window_size,
            format,
            dctx,
        })
    }

    #[args(ifh, ofh, read_size = "None", write_size = "None")]
    fn copy_stream(
        &self,
        ifh: &PyAny,
        ofh: &PyAny,
        read_size: Option<usize>,
        write_size: Option<usize>,
    ) -> PyResult<()> {
        Err(PyNotImplementedError::new_err(()))
    }

    #[args(buffer, max_output_size = "0")]
    fn decompress<'p>(
        &mut self,
        py: Python<'p>,
        buffer: PyBuffer<u8>,
        max_output_size: usize,
    ) -> PyResult<&'p PyBytes> {
        self.setup_dctx(py, true)?;

        let output_size =
            unsafe { zstd_sys::ZSTD_getFrameContentSize(buffer.buf_ptr(), buffer.len_bytes()) };

        let output_buffer_size = if output_size == zstd_sys::ZSTD_CONTENTSIZE_ERROR as _ {
            return Err(ZstdError::new_err(
                "error determining content size from frame header",
            ));
        } else if output_size == 0 {
            return Ok(PyBytes::new(py, &[]));
        } else if output_size == zstd_sys::ZSTD_CONTENTSIZE_UNKNOWN as _ {
            if max_output_size == 0 {
                return Err(ZstdError::new_err(
                    "could not determine content size in frame header",
                ));
            }

            max_output_size
        } else {
            output_size as _
        };

        let mut dest_buffer: Vec<u8> = Vec::new();
        dest_buffer
            .try_reserve_exact(output_buffer_size)
            .map_err(|_| PyMemoryError::new_err(()))?;

        let mut out_buffer = zstd_sys::ZSTD_outBuffer {
            dst: dest_buffer.as_mut_ptr() as *mut _,
            size: dest_buffer.capacity(),
            pos: 0,
        };

        let mut in_buffer = zstd_sys::ZSTD_inBuffer {
            src: buffer.buf_ptr(),
            size: buffer.len_bytes(),
            pos: 0,
        };

        let zresult = unsafe {
            zstd_sys::ZSTD_decompressStream(
                self.dctx.0,
                &mut out_buffer as *mut _,
                &mut in_buffer as *mut _,
            )
        };
        if unsafe { zstd_sys::ZSTD_isError(zresult) } != 0 {
            Err(ZstdError::new_err(format!(
                "decompression error: {}",
                zstd_safe::get_error_name(zresult),
            )))
        } else if zresult != 0 {
            Err(ZstdError::new_err(
                "decompression error: did not decompress full frame",
            ))
        } else if output_size != 0 && out_buffer.pos != output_size as _ {
            Err(ZstdError::new_err(format!(
                "decompression error: decompressed {} bytes; expected {}",
                zresult, output_size
            )))
        } else {
            // TODO avoid memory copy
            unsafe { dest_buffer.set_len(out_buffer.pos) };
            Ok(PyBytes::new(py, &dest_buffer))
        }
    }

    fn decompress_content_dict_chain(&self, frames: &PyAny) -> PyResult<()> {
        Err(PyNotImplementedError::new_err(()))
    }

    #[args(write_size = "None")]
    fn decompressobj(&self, write_size: Option<usize>) -> PyResult<()> {
        Err(PyNotImplementedError::new_err(()))
    }

    fn memory_size(&self) -> PyResult<usize> {
        Ok(unsafe { zstd_sys::ZSTD_sizeof_DCtx(self.dctx.0) })
    }

    #[args(frames, decompressed_sizes = "None", threads = "0")]
    fn multi_decompress_to_buffer(
        &self,
        frames: &PyAny,
        decompressed_sizes: Option<&PyAny>,
        threads: usize,
    ) -> PyResult<()> {
        Err(PyNotImplementedError::new_err(()))
    }

    #[args(reader, read_size = "None", write_size = "None", skip_bytes = "None")]
    fn read_to_iter(
        &self,
        reader: &PyAny,
        read_size: Option<usize>,
        write_size: Option<usize>,
        skip_bytes: Option<usize>,
    ) -> PyResult<()> {
        Err(PyNotImplementedError::new_err(()))
    }

    #[args(
        source,
        read_size = "None",
        read_across_frames = "false",
        closefd = "true"
    )]
    fn stream_reader(
        &self,
        source: &PyAny,
        read_size: Option<usize>,
        read_across_frames: bool,
        closefd: bool,
    ) -> PyResult<()> {
        Err(PyNotImplementedError::new_err(()))
    }

    #[args(
        writer,
        write_size = "None",
        write_return_read = "true",
        closefd = "true"
    )]
    fn stream_writer(
        &self,
        writer: &PyAny,
        write_size: Option<usize>,
        write_return_read: bool,
        closefd: bool,
    ) -> PyResult<()> {
        Err(PyNotImplementedError::new_err(()))
    }
}

pub(crate) fn init_module(module: &PyModule) -> PyResult<()> {
    module.add_class::<ZstdDecompressor>()?;

    Ok(())
}