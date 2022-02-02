use editor::message_prelude::FrontendMessage;
use glam::Vec2;
use graphene::layers::style::PathStyle;
use js_sys::Float32Array;
use js_sys::Uint16Array;
use lyon::lyon_tessellation::VertexBuffers;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{HtmlCanvasElement, WebGl2RenderingContext, WebGlProgram, WebGlShader};

#[derive(Clone, Debug)]
pub struct RenderingContext {
	context: WebGl2RenderingContext,
	scale: f64,
	program: WebGlProgram,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
#[allow(unused_variables)]
pub struct VertexData {
	pos: [f32; 2],
	color: [f32; 4],
	zindex: f32,
	width: f32,
	flags: u32,
}

impl VertexData {
	pub const CLOSED: u32 = 0b0000_0001;
	pub const ROUNDED: u32 = 0b0000_0010;

	pub fn new(pos: Vec2, zindex: f32, width: f32, color: editor::Color, closed: bool) -> Self {
		let mut flags = 0;
		flags |= Self::ROUNDED;
		if closed {
			//flags |= Self::CLOSED;
		}

		Self {
			pos: pos.into(),
			color: [color.r(), color.g(), color.b(), color.a()],
			zindex,
			width,
			flags,
		}
	}
}

pub fn window() -> web_sys::Window {
	web_sys::window().expect("no global `window` exists")
}

fn request_animation_frame(f: &Closure<dyn FnMut()>) {
	window().request_animation_frame(f.as_ref().unchecked_ref()).expect("should register `requestAnimationFrame` OK");
}

pub fn document() -> web_sys::Document {
	window().document().expect("should have a document on window")
}

fn canvas() -> HtmlCanvasElement {
	let canvas = document().query_selector(".rendering-canvas").unwrap().unwrap();
	canvas.dyn_into::<HtmlCanvasElement>().unwrap()
}

#[derive(Clone, Serialize, Deserialize)]
struct CanvasOptions {
	antialias: bool,
	premultipliedAlpha: bool,
}

impl RenderingContext {
	pub fn new() -> Result<Self, JsValue> {
		let scale = window().device_pixel_ratio();
		let map = js_sys::Map::new();
		map.set(&JsValue::from_str("premultipliedalpha"), &JsValue::from_str("false"));
		let options = CanvasOptions {
			antialias: false,
			premultipliedAlpha: false,
		};

		let context = canvas()
			.get_context_with_context_options("webgl2", &JsValue::from_serde(&options).unwrap())
			.unwrap()
			.unwrap()
			.dyn_into::<WebGl2RenderingContext>()?;
		//let context = canvas().get_context("webgl2").unwrap().unwrap().dyn_into::<WebGl2RenderingContext>()?;

		let vert_shader = compile_shader(&context, WebGl2RenderingContext::VERTEX_SHADER, include_str!("../shaders/shader.vert"))?;

		let frag_shader = compile_shader(&context, WebGl2RenderingContext::FRAGMENT_SHADER, include_str!("../shaders/shader.frag"))?;
		let program = link_program(&context, &vert_shader, &frag_shader)?;
		context.use_program(Some(&program));
		context.viewport(0, 0, canvas().width() as i32, canvas().height() as i32);

		let f = Rc::new(RefCell::new(None));
		let g = f.clone();

		*g.borrow_mut() = Some(Closure::wrap(Box::new(move || {
			super::JS_EDITOR_HANDLES.with(|instances| {
				let handles = instances.borrow();
				super::EDITOR_INSTANCES.with(|editors| {
					let editors = editors.borrow();
					if let Some((editor, handle)) = editors.values().zip(handles.values()).find(|(_, h)| h.renderer.is_some()) {
						let document = editor.dispatcher.message_handlers.portfolio_message_handler.active_document();
						let lines = document.overlays_message_handler.overlays_graphene_document.root.line_iter();
						let svg = document.graphene_document.root.cache.clone();
						if let Some(renderer) = handle.renderer.as_ref() {
							handle.send_frontend_message_to_js(FrontendMessage::UpdateDocumentArtwork { svg });
							renderer.draw_paths(lines);
						}
					}
				});
			});
			// Schedule ourself for another requestAnimationFrame callback.
			request_animation_frame(f.borrow().as_ref().unwrap());
		}) as Box<dyn FnMut()>));

		let context = Self { context, scale, program };
		context.init_buffer(VertexBuffers::new());
		request_animation_frame(g.borrow().as_ref().unwrap());
		Ok(context)
	}
	pub fn draw_paths(&self, lines: impl Iterator<Item = (lyon::path::Path, PathStyle, u32)>) {
		let mut geometry: VertexBuffers<VertexData, u16> = VertexBuffers::new();

		use lyon::tessellation::FillTessellator;
		let mut tessellator = FillTessellator::new();
		for (segments, style, depth) in lines {
			let stroke = style.stroke().unwrap();
			use lyon::tessellation::{BuffersBuilder, FillOptions, FillVertex};

			// Will contain the result of the tessellation.

			{
				// Compute the tessellation.
				tessellator
					.tessellate_path(
						&segments,
						&FillOptions::default(),
						&mut BuffersBuilder::new(&mut geometry, |vertex: FillVertex| {
							VertexData::new(
								Vec2::new(vertex.position().x, vertex.position().y),
								depth as f32 / 100.,
								stroke.weight() as f32 * 10.,
								stroke.color().unwrap_or_default(),
								style.fill().is_some(),
							)
						}),
					)
					.unwrap();
			}
		}

		self.draw(geometry);
		//self.draw_lines(&[(500.7, 500.7, 3100.7, 2700.7)]);
	}

	fn viewport_transform(&self) -> glam::Affine2 {
		let transform = glam::Affine2::from_scale(2. * Vec2::new(canvas().width() as f32, canvas().height() as f32).recip());
		let transform = glam::Affine2::from_scale(self.scale as f32 * Vec2::new(1., -1.)) * transform;
		glam::Affine2::from_translation(Vec2::new(-1., 1.)) * transform
	}

	fn update_buffer(&self, vertex_data: VertexBuffers<VertexData, u16>) -> Result<(), JsValue> {
		let vertices = vertex_data.vertices;
		let indices = vertex_data.indices;

		self.context.viewport(0, 0, canvas().width() as i32, canvas().height() as i32);
		let float_size = std::mem::size_of::<f32>() as i32;
		let u16_size = std::mem::size_of::<u16>() as i32;
		let vertex_size = std::mem::size_of::<VertexData>() as i32;

		let vertices_array = {
			let memory_buffer = wasm_bindgen::memory().dyn_into::<js_sys::WebAssembly::Memory>()?.buffer();
			let location: u32 = vertices.as_ptr() as u32 / float_size as u32;
			Float32Array::new(&memory_buffer).subarray(location, location + (vertices.len() * (vertex_size / float_size) as usize) as u32)
		};

		let indices_array = {
			let memory_buffer = wasm_bindgen::memory().dyn_into::<js_sys::WebAssembly::Memory>()?.buffer();
			let location: u32 = indices.as_ptr() as u32 / u16_size as u32;
			Float32Array::new(&memory_buffer).subarray(location, location + indices.len() as u32)
		};

		self.context
			.buffer_data_with_array_buffer_view(WebGl2RenderingContext::ARRAY_BUFFER, &vertices_array, WebGl2RenderingContext::DYNAMIC_DRAW);
		self.context
			.buffer_data_with_array_buffer_view(WebGl2RenderingContext::ELEMENT_ARRAY_BUFFER, &indices_array, WebGl2RenderingContext::DYNAMIC_DRAW);

		let transform = self.viewport_transform();

		let matrix_location = self.context.get_uniform_location(&self.program, "matrix");
		self.context.uniform_matrix3x2fv_with_f32_array(matrix_location.as_ref(), false, &transform.to_cols_array());

		let resolution_location = self.context.get_uniform_location(&self.program, "canvas_resolution");
		self.context
			.uniform2fv_with_f32_array(resolution_location.as_ref(), &[canvas().width() as f32, canvas().height() as f32]);

		Ok(())
	}

	fn init_buffer(&self, vertex_data: VertexBuffers<VertexData, u16>) -> Result<(), JsValue> {
		self.context.viewport(0, 0, canvas().width() as i32, canvas().height() as i32);
		let float_size = std::mem::size_of::<f32>() as i32;
		let u16_size = std::mem::size_of::<u16>() as i32;
		let vertex_size = std::mem::size_of::<VertexData>() as i32;

		let vertices = vertex_data.vertices;
		let indices = vertex_data.indices;

		let vertices_array = {
			let memory_buffer = wasm_bindgen::memory().dyn_into::<js_sys::WebAssembly::Memory>()?.buffer();
			let location: u32 = vertices.as_ptr() as u32 / float_size as u32;
			Float32Array::new(&memory_buffer).subarray(location, location + (vertices.len() * (vertex_size / float_size) as usize) as u32)
		};

		let vertex_buffer = self.context.create_buffer().ok_or("Failed to create buffer")?;
		self.context.bind_buffer(WebGl2RenderingContext::ARRAY_BUFFER, Some(&vertex_buffer));

		let indices_array = {
			let memory_buffer = wasm_bindgen::memory().dyn_into::<js_sys::WebAssembly::Memory>()?.buffer();
			let location: u32 = indices.as_ptr() as u32 / u16_size as u32;
			Float32Array::new(&memory_buffer).subarray(location, location + indices.len() as u32)
		};

		let index_buffer = self.context.create_buffer().ok_or("Failed to create buffer")?;
		self.context.bind_buffer(WebGl2RenderingContext::ELEMENT_ARRAY_BUFFER, Some(&index_buffer));
		// Create an empty buffer object to store the vertex buffer

		// Note that `Float32Array::view` is somewhat dangerous (hence the
		// `unsafe`!). This is creating a raw view into our module's
		// `WebAssembly.Memory` buffer, but if we allocate more pages for ourself
		// (aka do a memory allocation in Rust) it'll cause the buffer to change,
		// causing the `Float32Array` to be invalid.
		//
		// As a result, after `Float32Array::view` we have to be very careful not to
		// do any memory allocations before it's dropped.
		//let vertices = std::mem::transmute(&vertices[..]);
		//log::debug!("vertices: {vertices:?}");

		let transform = self.viewport_transform();

		let matrix_location = self.context.get_uniform_location(&self.program, "matrix");
		self.context.uniform_matrix3x2fv_with_f32_array(matrix_location.as_ref(), false, &transform.to_cols_array());

		let resolution_location = self.context.get_uniform_location(&self.program, "canvas_resolution");
		self.context
			.uniform2fv_with_f32_array(resolution_location.as_ref(), &[canvas().width() as f32, canvas().height() as f32]);

		self.context
			.buffer_data_with_array_buffer_view(WebGl2RenderingContext::ARRAY_BUFFER, &vertices_array, WebGl2RenderingContext::DYNAMIC_DRAW);

		self.context
			.buffer_data_with_array_buffer_view(WebGl2RenderingContext::ELEMENT_ARRAY_BUFFER, &indices_array, WebGl2RenderingContext::DYNAMIC_DRAW);
		let vao = self.context.create_vertex_array().ok_or("Could not create vertex array object")?;
		self.context.bind_vertex_array(Some(&vao));

		//log::debug!("positon location: {position_attribute_location:?}");
		//log::debug!("line location: {line_attribute_location:?}");

		let line_start_attribute_location = self.context.get_attrib_location(&self.program, "pos");
		let color_attribute_location = self.context.get_attrib_location(&self.program, "line_color");
		let zindex_attribute_location = self.context.get_attrib_location(&self.program, "line_zindex");
		let width_attribute_location = self.context.get_attrib_location(&self.program, "line_width");
		let flags_attribute_location = self.context.get_attrib_location(&self.program, "line_flags");

		assert_eq!(line_start_attribute_location, 0);
		assert_eq!(color_attribute_location, 2);
		assert_eq!(zindex_attribute_location, 3);
		assert_eq!(width_attribute_location, 4);
		assert_eq!(flags_attribute_location, 5);

		self.context.enable_vertex_attrib_array(line_start_attribute_location as u32);
		self.context.vertex_attrib_pointer_with_i32(0, 2, WebGl2RenderingContext::FLOAT, false, vertex_size, 0);
		self.context.vertex_attrib_divisor(line_start_attribute_location as u32, 1);
		self.context.enable_vertex_attrib_array(color_attribute_location as u32);
		self.context.vertex_attrib_pointer_with_i32(2, 4, WebGl2RenderingContext::FLOAT, false, vertex_size, float_size * 2);
		self.context.vertex_attrib_divisor(color_attribute_location as u32, 1);
		self.context.enable_vertex_attrib_array(zindex_attribute_location as u32);
		self.context.vertex_attrib_pointer_with_i32(3, 1, WebGl2RenderingContext::FLOAT, false, vertex_size, float_size * 6);
		self.context.vertex_attrib_divisor(zindex_attribute_location as u32, 1);
		self.context.enable_vertex_attrib_array(width_attribute_location as u32);
		self.context.vertex_attrib_pointer_with_i32(4, 1, WebGl2RenderingContext::FLOAT, false, vertex_size, float_size * 7);
		self.context.vertex_attrib_divisor(width_attribute_location as u32, 1);
		self.context.enable_vertex_attrib_array(flags_attribute_location as u32);
		self.context.vertex_attrib_i_pointer_with_i32(5, 1, WebGl2RenderingContext::UNSIGNED_INT, vertex_size, float_size * 8);
		self.context.vertex_attrib_divisor(flags_attribute_location as u32, 1);
		Ok(())
	}

	pub fn draw(&self, vertex_data: VertexBuffers<VertexData, u16>) -> Result<(), JsValue> {
		let vert_count = vertex_data.indices.len() as i32;
		log::trace!("vert count: {}", vert_count);
		self.update_buffer(vertex_data);
		draw(&self.context, vert_count);

		Ok(())
	}
}

fn draw(context: &WebGl2RenderingContext, vert_count: i32) {
	context.clear_color(0.0, 0.0, 0.0, 0.0);
	//context.clear_color(1.0, 0.5, 1.0, 1.0);
	context.clear(WebGl2RenderingContext::COLOR_BUFFER_BIT);
	context.disable(WebGl2RenderingContext::DEPTH_TEST);
	context.enable(WebGl2RenderingContext::BLEND);

	context.blend_func_separate(
		WebGl2RenderingContext::SRC_ALPHA,
		WebGl2RenderingContext::ONE_MINUS_SRC_ALPHA,
		WebGl2RenderingContext::ONE,
		WebGl2RenderingContext::ONE_MINUS_SRC_ALPHA,
	);
	//context.depth_func(WebGl2RenderingContext::LESS);

	context.draw_elements_with_i32(WebGl2RenderingContext::TRIANGLES, vert_count, WebGl2RenderingContext::UNSIGNED_SHORT, 0);
	//context.draw_arrays(WebGl2RenderingContext::TRIANGLES, 0, vert_count);
}

pub fn compile_shader(context: &WebGl2RenderingContext, shader_type: u32, source: &str) -> Result<WebGlShader, String> {
	let shader = context.create_shader(shader_type).ok_or_else(|| String::from("Unable to create shader object"))?;
	context.shader_source(&shader, source);
	context.compile_shader(&shader);

	if context.get_shader_parameter(&shader, WebGl2RenderingContext::COMPILE_STATUS).as_bool().unwrap_or(false) {
		Ok(shader)
	} else {
		Err(context.get_shader_info_log(&shader).unwrap_or_else(|| String::from("Unknown error creating shader")))
	}
}

pub fn link_program(context: &WebGl2RenderingContext, vert_shader: &WebGlShader, frag_shader: &WebGlShader) -> Result<WebGlProgram, String> {
	let program = context.create_program().ok_or_else(|| String::from("Unable to create shader object"))?;

	context.attach_shader(&program, vert_shader);
	context.attach_shader(&program, frag_shader);
	context.link_program(&program);

	if context.get_program_parameter(&program, WebGl2RenderingContext::LINK_STATUS).as_bool().unwrap_or(false) {
		Ok(program)
	} else {
		Err(context.get_program_info_log(&program).unwrap_or_else(|| String::from("Unknown error creating program object")))
	}
}
