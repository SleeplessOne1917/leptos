use crate::{
    generate_route_list::RouteList,
    location::{Location, RequestUrl},
    matching::{
        MatchInterface, MatchNestedRoutes, PossibleRouteMatch, RouteMatchId,
        Routes,
    },
    ChooseView, MatchParams, Method, Params, PathSegment, RouteListing,
    SsrMode,
};
use core::marker::PhantomData;
use either_of::*;
use once_cell::sync::Lazy;
use or_poisoned::OrPoisoned;
use reactive_graph::{
    computed::{ArcMemo, Memo},
    effect::RenderEffect,
    owner::{use_context, Owner},
    signal::ArcRwSignal,
    traits::{Get, Read, Set, Track},
};
use send_wrapper::SendWrapper;
use std::{
    any::Any,
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::VecDeque,
    fmt::Debug,
    future::{ready, Ready},
    iter,
    rc::Rc,
    sync::{Arc, RwLock},
};
use tachys::{
    html::attribute::Attribute,
    hydration::Cursor,
    renderer::{dom::Dom, Renderer},
    ssr::StreamBuilder,
    view::{
        add_attr::AddAnyAttr,
        any_view::{AnyView, AnyViewState, IntoAny},
        either::EitherState,
        Mountable, Position, PositionState, Render, RenderHtml,
    },
};

#[derive(Debug)]
pub struct Router<Rndr, Loc, Children, FallbackFn> {
    base: Option<Cow<'static, str>>,
    location: PhantomData<Loc>,
    pub routes: Routes<Children>,
    fallback: FallbackFn,
}

impl<Rndr, Loc, Children, FallbackFn, Fallback>
    Router<Rndr, Loc, Children, FallbackFn>
where
    Loc: Location,
    Rndr: Renderer,
    FallbackFn: Fn() -> Fallback,
{
    pub fn new(
        routes: Routes<Children>,
        fallback: FallbackFn,
    ) -> Router<Rndr, Loc, Children, FallbackFn> {
        Self {
            base: None,
            location: PhantomData,
            routes,
            fallback,
        }
    }

    pub fn new_with_base(
        base: impl Into<Cow<'static, str>>,
        routes: Routes<Children>,
        fallback: FallbackFn,
    ) -> Router<Rndr, Loc, Children, FallbackFn> {
        Self {
            base: Some(base.into()),
            location: PhantomData,
            routes,
            fallback,
        }
    }
}

impl<Rndr, Loc, Children, FallbackFn, Fallback>
    Router<Rndr, Loc, Children, FallbackFn>
where
    FallbackFn: Fn() -> Fallback,
    Rndr: Renderer,
{
    pub fn fallback(&self) -> Fallback {
        (self.fallback)()
    }
}

pub struct 
where
    
{
    pub params: ArcMemo<Params>,
    pub outlet: Outlet,
}

impl<Rndr, Loc, FallbackFn, Fallback, Children> Render
    for Router<Rndr, Loc, Children, FallbackFn>
where
    Loc: Location,
    FallbackFn: Fn() -> Fallback + 'static,
    Fallback: Render,
    Children: MatchNestedRoutes + 'static,
    Fallback::State: 'static,
    Rndr: Renderer + 'static,
    Children::Match: std::fmt::Debug,
    <Children::Match as MatchInterface>::Child: std::fmt::Debug,
{
    type State = RenderEffect<
        EitherState<
            <NestedRouteView<Children::Match> as Render>::State,
            <Fallback as Render>::State,
            Rndr,
        >,
    >;

    fn build(self) -> Self::State {
        let location = Loc::new().unwrap(); // TODO
        location.init(self.base);
        let url = location.as_url().clone();
        let path = ArcMemo::new({
            let url = url.clone();
            move |_| url.read().path().to_string()
        });
        let search_params = ArcMemo::new({
            let url = url.clone();
            move |_| url.read().search_params().clone()
        });
        let outer_owner =
            Owner::current().expect("creating Router, but no Owner was found");

        RenderEffect::new(move |prev: Option<EitherState<_, _, _>>| {
            let path = path.read();
            let new_match = self.routes.match_route(&path);

            if let Some(mut prev) = prev {
                if let Some(new_match) = new_match {
                    match &mut prev.state {
                        Either::Left(prev) => {
                            rebuild_nested(&outer_owner, prev, new_match);
                        }
                        Either::Right(_) => {
                            Either::<_, Fallback>::Left(NestedRouteView::new(
                                &outer_owner,
                                new_match,
                            ))
                            .rebuild(&mut prev);
                        }
                    }
                } else {
                    Either::<NestedRouteView<Children::Match>, _>::Right(
                        (self.fallback)(),
                    )
                    .rebuild(&mut prev);
                }
                prev
            } else {
                match new_match {
                    Some(matched) => Either::Left(NestedRouteView::new(
                        &outer_owner,
                        matched,
                    )),
                    _ => Either::Right((self.fallback)()),
                }
                .build()
            }
        })
    }

    fn rebuild(self, state: &mut Self::State) {}
}

impl<Rndr, Loc, FallbackFn, Fallback, Children> RenderHtml
    for Router<Rndr, Loc, Children, FallbackFn>
where
    Loc: Location + Send,
    FallbackFn: Fn() -> Fallback + Send + 'static,
    Fallback: RenderHtml,
    Children: MatchNestedRoutes + Send + 'static,
    Children::View: RenderHtml,
    /*View: Render + IntoAny + 'static,
    View::State: 'static,*/
    Fallback: RenderHtml,
    Fallback::State: 'static,
    Rndr: Renderer + 'static,
    Children::Match: std::fmt::Debug,
    <Children::Match as MatchInterface>::Child: std::fmt::Debug,
{
    type AsyncOutput = Self;

    // TODO probably pick a max length here
    const MIN_LENGTH: usize = Children::View::MIN_LENGTH;

    async fn resolve(self) -> Self::AsyncOutput {
        self
    }

    fn to_html_with_buf(self, buf: &mut String, position: &mut Position, escape: bool, mark_branches: bool) {
        // if this is being run on the server for the first time, generating all possible routes
        if RouteList::is_generating() {
            // add routes
            let (base, routes) = self.routes.generate_routes();
            let mut routes = routes
                .into_iter()
                .map(|data| {
                    let path = base
                        .into_iter()
                        .flat_map(|base| {
                            iter::once(PathSegment::Static(
                                base.to_string().into(),
                            ))
                        })
                        .chain(data.segments)
                        .collect::<Vec<_>>();
                    // TODO add non-defaults for mode, etc.
                    RouteListing::new(
                        path,
                        data.ssr_mode,
                        data.methods,
                        None,
                    )
                })
                .collect::<Vec<_>>();

            // add fallback
            // TODO fix: causes overlapping route issues on Axum
            /*routes.push(RouteListing::new(
                [PathSegment::Static(
                    base.unwrap_or_default().to_string().into(),
                )],
                SsrMode::Async,
                [
                    Method::Get,
                    Method::Post,
                    Method::Put,
                    Method::Patch,
                    Method::Delete,
                ],
                None,
            ));*/

            RouteList::register(RouteList::from(routes));
        } else {
            let outer_owner = Owner::current()
                .expect("creating Router, but no Owner was found");
            let url = use_context::<RequestUrl>()
                .expect("could not find request URL in context");
            // TODO base
            let url =
                RequestUrl::parse(url.as_ref()).expect("could not parse URL");
            // TODO query params
            let new_match = self.routes.match_route(url.path());
            /*match new_match {
                Some(matched) => {
                    Either::Left(NestedRouteView::new(&outer_owner, matched))
                }
                _ => Either::Right((self.fallback)()),
            }
            .to_html_with_buf(buf, position)*/
        }
    }

    fn to_html_async_with_buf<const OUT_OF_ORDER: bool>(
        self,
        buf: &mut StreamBuilder, position: &mut Position, escape: bool, mark_branches: bool) where
        Self: Sized,
    {
        let outer_owner =
            Owner::current().expect("creating Router, but no Owner was found");
        let url = use_context::<RequestUrl>()
            .expect("could not find request URL in context");
        // TODO base
        let url = RequestUrl::parse(url.as_ref()).expect("could not parse URL");
        // TODO query params
        let new_match = self.routes.match_route(url.path());
        /*match new_match {
            Some(matched) => {
                Either::Left(NestedRouteView::new(&outer_owner, matched))
            }
            _ => Either::Right((self.fallback)()),
        }
        .to_html_async_with_buf::<OUT_OF_ORDER>(buf, position, escape)*/
    }

    fn hydrate<const FROM_SERVER: bool>(
        self,
        cursor: &Cursor,
        position: &PositionState,
    ) -> Self::State {
        let location = Loc::new().unwrap(); // TODO
        location.init(self.base);
        let url = location.as_url().clone();
        let path = ArcMemo::new({
            let url = url.clone();
            move |_| url.read().path().to_string()
        });
        let search_params = ArcMemo::new({
            let url = url.clone();
            move |_| url.read().search_params().clone()
        });
        let outer_owner =
            Owner::current().expect("creating Router, but no Owner was found");

        let cursor = cursor.clone();
        let position = position.clone();
        RenderEffect::new(move |prev: Option<EitherState<_, _, _>>| {
            let path = path.read();
            let new_match = self.routes.match_route(&path);

            if let Some(mut prev) = prev {
                if let Some(new_match) = new_match {
                    match &mut prev.state {
                        Either::Left(prev) => {
                            rebuild_nested(&outer_owner, prev, new_match);
                        }
                        Either::Right(_) => {
                            Either::<_, Fallback>::Left(NestedRouteView::new(
                                &outer_owner,
                                new_match,
                            ))
                            .rebuild(&mut prev);
                        }
                    }
                } else {
                    Either::<NestedRouteView<Children::Match>, _>::Right(
                        (self.fallback)(),
                    )
                    .rebuild(&mut prev);
                }
                prev
            } else {
                /*match new_match {
                    Some(matched) => {
                        Either::Left(NestedRouteView::new_hydrate(
                            &outer_owner,
                            matched,
                            &cursor,
                            &position,
                        ))
                    }
                    _ => Either::Right((self.fallback)()),
                }
                .hydrate::<true>(&cursor, &position)*/
                todo!()
            }
        })
    }
}

pub struct NestedRouteView<Matcher, R>
where
    Matcher: MatchInterface,
    
{
    id: RouteMatchId,
    owner: Owner,
    params: ArcRwSignal<Params>,
    outlets: VecDeque<Outlet>,
    view: Matcher::View,
    ty: PhantomData<(Matcher, R)>,
}

impl<Matcher> NestedRouteView<Matcher>
where
    Matcher: MatchInterface + MatchParams,
    Matcher::Child: 'static,
    Matcher::View: 'static,
    Rndr: Renderer + 'static,
{
    pub fn new(outer_owner: &Owner, route_match: Matcher) -> Self {
        // keep track of all outlets, for diffing
        let mut outlets = VecDeque::new();

        // build this view
        let owner = outer_owner.child();
        let id = route_match.as_id();
        let params =
            ArcRwSignal::new(route_match.to_params().into_iter().collect());
        let (view, child) = route_match.into_view_and_child();

        let outlet = child
            .map(|child| get_inner_view(&mut outlets, &owner, child))
            .unwrap_or_default();

        let  = RouteData {
            params: ArcMemo::new({
                let params = params.clone();
                move |_| params.get()
            }),
            outlet,
        };
        let view = owner.with(|| view.choose());

        Self {
            id,
            owner,
            params,
            outlets,
            view,
            ty: PhantomData,
        }
    }

    pub fn new_hydrate(
        outer_owner: &Owner,
        route_match: Matcher,
        cursor: &Cursor,
        position: &PositionState,
    ) -> Self {
        // keep track of all outlets, for diffing
        let mut outlets = VecDeque::new();

        // build this view
        let owner = outer_owner.child();
        let id = route_match.as_id();
        let params =
            ArcRwSignal::new(route_match.to_params().into_iter().collect());
        let (view, child) = route_match.into_view_and_child();

        let outlet = child
            .map(|child| {
                get_inner_view_hydrate(
                    &mut outlets,
                    &owner,
                    child,
                    cursor,
                    position,
                )
            })
            .unwrap_or_default();

        let  = RouteData {
            params: ArcMemo::new({
                let params = params.clone();
                move |_| params.get()
            }),
            outlet,
        };
        let view = owner.with(|| view.choose());

        Self {
            id,
            owner,
            params,
            outlets,
            view,
            ty: PhantomData,
        }
    }
}

pub struct NestedRouteState<Matcher>
where
    Matcher: MatchInterface,
    Rndr: Renderer + 'static,
{
    id: RouteMatchId,
    owner: Owner,
    params: ArcRwSignal<Params>,
    view: <Matcher::View as Render>::State,
    outlets: VecDeque<Outlet>,
}

fn get_inner_view<Match, R>(
    outlets: &mut VecDeque<Outlet>,
    parent: &Owner,
    route_match: Match,
) -> Outlet
where
    Match: MatchInterface + MatchParams,
    
{
    let owner = parent.child();
    let id = route_match.as_id();
    let params =
        ArcRwSignal::new(route_match.to_params().into_iter().collect());
    let (view, child) = route_match.into_view_and_child();
    let outlet = child
        .map(|child| get_inner_view(outlets, &owner, child))
        .unwrap_or_default();

    /*let view = Arc::new(Lazy::new({
        let owner = owner.clone();
        let params = params.clone();
        Box::new(move || {
            RwLock::new(Some(
                owner
                    .with(|| {
                        view.choose(RouteData {
                            params: ArcMemo::new(move |_| params.get()),
                            outlet,
                        })
                    })
                    .into_any(),
            ))
        }) as Box<dyn FnOnce() -> RwLock<Option<AnyView>>>
    }));
    let inner = Arc::new(RwLock::new(OutletStateInner {
        html_len: {
            let view = Arc::clone(&view);
            Box::new(move || view.read().or_poisoned().html_len())
        },
        view: Arc::clone(&view),
        state: Lazy::new(Box::new(move || view.take().unwrap().build())),
    }));*/

    let outlet = Outlet {
        id,
        owner,
        params,
        rndr: PhantomData, //inner,
    };
    outlets.push_back(outlet.clone());
    outlet
}

fn get_inner_view_hydrate<Match, R>(
    outlets: &mut VecDeque<Outlet>,
    parent: &Owner,
    route_match: Match,
    cursor: &Cursor,
    position: &PositionState,
) -> Outlet
where
    Match: MatchInterface + MatchParams,
    
{
    let owner = parent.child();
    let id = route_match.as_id();
    let params =
        ArcRwSignal::new(route_match.to_params().into_iter().collect());
    let (view, child) = route_match.into_view_and_child();
    let outlet = child
        .map(|child| get_inner_view(outlets, &owner, child))
        .unwrap_or_default();

    let view = Arc::new(Lazy::new({
        let owner = owner.clone();
        let params = params.clone();
        Box::new(move || {
            RwLock::new(Some(
                owner
                    .with(|| {
                        view.choose(RouteData {
                            params: ArcMemo::new(move |_| params.get()),
                            outlet,
                        })
                    })
                    .into_any(),
            ))
        }) as Box<dyn FnOnce() -> RwLock<Option<AnyView>>>
    }));
    let inner = Arc::new(RwLock::new(OutletStateInner {
        html_len: Box::new({
            let view = Arc::clone(&view);
            move || view.read().or_poisoned().html_len()
        }),
        view: Arc::clone(&view),
        state: Lazy::new(Box::new({
            let cursor = cursor.clone();
            let position = position.clone();
            move || view.take().unwrap().hydrate::<true>(&cursor, &position)
        })),
    }));

    let outlet = Outlet {
        id,
        owner,
        params,
        rndr: PhantomData,
        inner,
    };
    outlets.push_back(outlet.clone());
    outlet
}

#[derive(Debug)]
pub struct Outlet
where
    R: Renderer + Send + 'static,
{
    id: RouteMatchId,
    owner: Owner,
    params: ArcRwSignal<Params>,
    rndr: PhantomData,
    inner: OutletInner,
}

pub enum OutletInner where R: Renderer + 'static {
    Server {
        html_len: Box<dyn Fn() -> usize + Send + Sync>,
        view: Box<
    }
}

/*

    html_len: Box<dyn Fn() -> usize + Send + Sync>,
    view: Arc<
        Lazy<
            RwLock<Option<AnyView>>,
            Box<dyn FnOnce() -> RwLock<Option<AnyView>> + Send + Sync>,
        >,
    >,
    state: Lazy<
        SendWrapper<AnyViewState>,
        Box<dyn FnOnce() -> SendWrapper<AnyViewState> + Send + Sync>,
    >,
    */

impl<R: Renderer> Debug for OutletInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutletInner").finish_non_exhaustive()
    }
}

impl Default for OutletInner
where
    
{
    fn default() -> Self {
        let view =
            Arc::new(Lazy::new(Box::new(|| RwLock::new(Some(().into_any())))
                as Box<dyn FnOnce() -> RwLock<Option<AnyView>>>));
        Self {
            html_len: Box::new(|| 0),
            view,
            state: Lazy::new(Box::new(|| ().into_any().build())),
        }
    }
}

impl Clone for Outlet
where
    
{
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            owner: self.owner.clone(),
            params: self.params.clone(),
            rndr: PhantomData,
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Default for Outlet
where
    
{
    fn default() -> Self {
        Self {
            id: RouteMatchId(0),
            owner: Owner::current().unwrap(),
            params: ArcRwSignal::new(Params::new()),
            rndr: PhantomData, //inner: Default::default(),
        }
    }
}

impl Render for Outlet
where
    
{
    type State = Outlet;

    fn build(self) -> Self::State {
        self
    }

    fn rebuild(self, state: &mut Self::State) {
        todo!()
    }
}

impl RenderHtml for Outlet
where
    
{
    type AsyncOutput = Self;

    const MIN_LENGTH: usize = 0; // TODO

    async fn resolve(self) -> Self::AsyncOutput {
        self
    }

    fn html_len(&self) -> usize {
        todo!()
        //(self.inner.read().or_poisoned().html_len)()
    }

    fn to_html_with_buf(self, buf: &mut String, position: &mut Position, escape: bool, mark_branches: bool) {
        /*let view = self.inner.read().or_poisoned().view.take().unwrap();
        view.to_html_with_buf(buf, position);*/
    }

    fn to_html_async_with_buf<const OUT_OF_ORDER: bool>(
        self,
        buf: &mut StreamBuilder, position: &mut Position, escape: bool, mark_branches: bool) where
        Self: Sized,
    {
        /*let view = self
            .inner
            .read()
            .or_poisoned()
            .view
            .write()
            .or_poisoned()
            .take()
            .unwrap();
        view.to_html_async_with_buf::<OUT_OF_ORDER>(buf, position, escape);*/
    }

    fn hydrate<const FROM_SERVER: bool>(
        self,
        cursor: &Cursor,
        position: &PositionState,
    ) -> Self::State {
        todo!()
        /*let view = self
            .inner
            .read()
            .or_poisoned()
            .view
            .write()
            .or_poisoned()
            .take()
            .unwrap();
        let state = view.hydrate::<FROM_SERVER>(cursor, position);
        self*/
    }
}

/*pub struct OutletStateInner
where
    
{
    html_len: Box<dyn Fn() -> usize + Send + Sync>,
    view: Arc<
        Lazy<
            RwLock<Option<AnyView>>,
            Box<dyn FnOnce() -> RwLock<Option<AnyView>> + Send + Sync>,
        >,
    >,
    state: Lazy<
        SendWrapper<AnyViewState>,
        Box<dyn FnOnce() -> SendWrapper<AnyViewState> + Send + Sync>,
    >,
}


impl<R: Renderer> Debug for OutletStateInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutletStateInner").finish_non_exhaustive()
    }
}

impl Default for OutletStateInner
where
    
{
    fn default() -> Self {
        let view =
            Arc::new(Lazy::new(Box::new(|| RwLock::new(Some(().into_any())))
                as Box<dyn FnOnce() -> RwLock<Option<AnyView>>>));
        Self {
            html_len: Box::new(|| 0),
            view,
            state: Lazy::new(Box::new(|| ().into_any().build())),
        }
    }
}
*/

impl Mountable for Outlet
where
    
{
    fn unmount(&mut self) {
        todo!()
        //self.inner.write().or_poisoned().state.unmount();
    }

    fn mount(
        &mut self,
        parent: &<R as Renderer>::Element,
        marker: Option<&<R as Renderer>::Node>,
    ) {
        self.inner.write().or_poisoned().state.mount(parent, marker);
    }

    fn insert_before_this(&self, 
        child: &mut dyn Mountable,
    ) -> bool {
        /*self.inner
        .write()
        .or_poisoned()
        .state
        .insert_before_this(child)*/
        todo!()
    }
}

fn rebuild_nested<Match, R>(
    outer_owner: &Owner,
    prev: &mut NestedRouteState<Match, R>,
    new_match: Match,
) where
    Match: MatchInterface + MatchParams + std::fmt::Debug,
    
{
    let mut items = 0;
    let NestedRouteState {
        id,
        owner,
        params,
        view,
        outlets,
    } = prev;

    if new_match.as_id() == *id {
        params.set(new_match.to_params().into_iter().collect::<Params>());
        let (_, child) = new_match.into_view_and_child();
        if let Some(child) = child {
            rebuild_inner(&mut items, outlets, child);
        } else {
            outlets.truncate(items);
        }
    } else {
        let new = NestedRouteView::new(outer_owner, new_match);
        new.rebuild(prev);
    }
}

fn rebuild_inner<Match, R>(
    items: &mut usize,
    outlets: &mut VecDeque<Outlet>,
    route_match: Match,
) where
    Match: MatchInterface + MatchParams,
    
{
    *items += 1;

    match outlets.pop_front() {
        None => todo!(),
        Some(mut prev) => {
            let prev_id = prev.id;
            let new_id = route_match.as_id();

            // we'll always update the params to the new params
            prev.params
                .set(route_match.to_params().into_iter().collect::<Params>());

            if new_id == prev_id {
                outlets.push_front(prev);
                let (_, child) = route_match.into_view_and_child();
                if let Some(child) = child {
                    // we still recurse to the children, because they may also have changed
                    rebuild_inner(items, outlets, child);
                } else {
                    outlets.truncate(*items);
                }
            } else {
                // we'll be updating the previous outlet before pushing it back onto the stack
                // update the ID to the ID of the new route
                prev.id = new_id;
                outlets.push_front(prev.clone());

                // if different routes are matched here, it means the rest of the tree is no longer
                // matched either
                outlets.truncate(*items);

                // we'll build a fresh tree instead
                let (view, child) = route_match.into_view_and_child();

                // first, let's add all the outlets that would be created by children
                let outlet = child
                    .map(|child| get_inner_view(outlets, &prev.owner, child))
                    .unwrap_or_default();

                // now, let's update the previou route at this point in the tree
                /*let mut prev_state = prev.inner.write().or_poisoned();
                let new_view = prev.owner.with_cleanup(|| {
                    view.choose(RouteData {
                        params: ArcMemo::new({
                            let params = prev.params.clone();
                            move |_| params.get()
                        }),
                        outlet,
                    })
                });

                new_view.into_any().rebuild(&mut prev_state.state);*/
                todo!()
            }
        }
    }
}

impl<Matcher, R> Render for NestedRouteView<Matcher, R>
where
    Matcher: MatchInterface,
    Matcher::View: Sized + 'static,
    
{
    type State = NestedRouteState<Matcher, R>;

    fn build(self) -> Self::State {
        let NestedRouteView {
            id,
            owner,
            params,
            outlets,
            view,
            ty,
        } = self;
        NestedRouteState {
            id,
            owner,
            outlets,
            params,
            view: view.build(),
        }
    }

    fn rebuild(self, state: &mut Self::State) {
        let NestedRouteView {
            id,
            owner,
            params,
            outlets,
            view,
            ty,
        } = self;
        state.id = id;
        state.owner = owner;
        state.params = params;
        state.outlets = outlets;
        view.rebuild(&mut state.view);
    }
}

impl<Matcher, R> RenderHtml for NestedRouteView<Matcher, R>
where
    Matcher: MatchInterface + Send,
    Matcher::View: Sized + 'static,
    
{
    type AsyncOutput = Self;

    const MIN_LENGTH: usize = Matcher::View::MIN_LENGTH;

    async fn resolve(self) -> Self::AsyncOutput {
        self
    }

    fn html_len(&self) -> usize {
        self.view.html_len()
    }

    fn to_html_with_buf(self, buf: &mut String, position: &mut Position, escape: bool, mark_branches: bool) {
        buf.reserve(self.html_len());
        self.view.to_html_with_buf(buf, position, escape);
    }

    fn to_html_async_with_buf<const OUT_OF_ORDER: bool>(
        self,
        buf: &mut StreamBuilder, position: &mut Position, escape: bool, mark_branches: bool) where
        Self: Sized,
    {
        buf.reserve(self.html_len());
        self.view
            .to_html_async_with_buf::<OUT_OF_ORDER>(buf, position, escape)
    }

    fn hydrate<const FROM_SERVER: bool>(
        self,
        cursor: &Cursor,
        position: &PositionState,
    ) -> Self::State {
        let NestedRouteView {
            id,
            owner,
            params,
            outlets,
            view,
            ty,
        } = self;
        NestedRouteState {
            id,
            owner,
            outlets,
            params,
            view: view.hydrate::<FROM_SERVER>(cursor, position),
        }
    }
}

impl<Matcher, R> Mountable for NestedRouteState<Matcher, R>
where
    Matcher: MatchInterface,
    
{
    fn unmount(&mut self) {
        self.view.unmount();
    }

    fn mount(&mut self, parent: &R::Element, marker: Option<&R::Node>) {
        self.view.mount(parent, marker);
    }

    fn insert_before_this(&self, 
        child: &mut dyn Mountable,
    ) -> bool {
        self.view.insert_before_this(child)
    }
}

impl<Rndr, Loc, FallbackFn, Fallback, Children, View> AddAnyAttr
    for Router<Rndr, Loc, Children, FallbackFn>
where
    Loc: Location,
    FallbackFn: Fn() -> Fallback,
    Fallback: Render,
    Children: MatchNestedRoutes,
     <<Children as MatchNestedRoutes>::Match as MatchInterface<
        Rndr,
    >>::View: ChooseView<Rndr, Output = View>,
    Rndr: Renderer + 'static,
    Router<Rndr, Loc, Children, FallbackFn>: RenderHtml,
{
    type Output<SomeNewAttr: Attribute> = Self;

    fn add_any_attr<NewAttr: Attribute>(
        self,
        attr: NewAttr,
    ) -> Self::Output<NewAttr>
    where
        Self::Output<NewAttr>: RenderHtml,
    {
        self
    }

    fn add_any_attr_by_ref<NewAttr: Attribute>(
        self,
        attr: &NewAttr,
    ) -> Self::Output<NewAttr>
    where
        Self::Output<NewAttr>: RenderHtml,
    {
        self
    }
}

#[derive(Debug)]
pub struct FlatRouter<Rndr, Loc, Children, FallbackFn> {
    base: Option<Cow<'static, str>>,
    location: PhantomData<Loc>,
    pub routes: Routes<Children>,
    fallback: FallbackFn,
}

impl<Rndr, Loc, Children, FallbackFn, Fallback>
    FlatRouter<Rndr, Loc, Children, FallbackFn>
where
    Loc: Location,
    Rndr: Renderer,
    FallbackFn: Fn() -> Fallback,
{
    pub fn new(
        routes: Routes<Children>,
        fallback: FallbackFn,
    ) -> FlatRouter<Rndr, Loc, Children, FallbackFn> {
        Self {
            base: None,
            location: PhantomData,
            routes,
            fallback,
        }
    }

    pub fn new_with_base(
        base: impl Into<Cow<'static, str>>,
        routes: Routes<Children>,
        fallback: FallbackFn,
    ) -> FlatRouter<Rndr, Loc, Children, FallbackFn> {
        Self {
            base: Some(base.into()),
            location: PhantomData,
            routes,
            fallback,
        }
    }
}
impl<Rndr, Loc, FallbackFn, Fallback, Children> Render
    for FlatRouter<Rndr, Loc, Children, FallbackFn>
where
    Loc: Location,
    FallbackFn: Fn() -> Fallback + 'static,
    Fallback: Render,
    Children: MatchNestedRoutes + 'static,
    Fallback::State: 'static,
    Rndr: Renderer + 'static,
{
    type State =
        RenderEffect<
            EitherState<
                <<Children::Match as MatchInterface>::View as Render<
                    Rndr,
                >>::State,
                <Fallback as Render>::State,
                Rndr,
            >,
        >;

    fn build(self) -> Self::State {
        let location = Loc::new().unwrap(); // TODO
        location.init(self.base);
        let url = location.as_url().clone();
        let path = ArcMemo::new({
            let url = url.clone();
            move |_| url.read().path().to_string()
        });
        let search_params = ArcMemo::new({
            let url = url.clone();
            move |_| url.read().search_params().clone()
        });
        let outer_owner =
            Owner::current().expect("creating Router, but no Owner was found");

        RenderEffect::new(move |prev: Option<EitherState<_, _, _>>| {
            let path = path.read();
            let new_match = self.routes.match_route(&path);

            if let Some(mut prev) = prev {
                if let Some(new_match) = new_match {
                    let params = ArcRwSignal::new(
                        new_match.to_params().into_iter().collect(),
                    );
                    #[allow(unused)]
                    let (view, child) = new_match.into_view_and_child();
                    #[cfg(debug_assertions)]
                    if child.is_some() {
                        panic!(
                            "FlatRouter should not be used with a route that \
                             has a child."
                        );
                    }

                    let  = RouteData {
                        params: ArcMemo::new({
                            let params = params.clone();
                            move |_| params.get()
                        }),
                        outlet: Default::default(),
                    };
                    let view = outer_owner.with(|| view.choose());
                    Either::Left::<_, Fallback>(view).rebuild(&mut prev);
                } else {
                    Either::<<Children::Match as MatchInterface>::View, _>::Right((self.fallback)()).rebuild(&mut prev);
                }
                prev
            } else {
                match new_match {
                    Some(matched) => {
                        let params = ArcRwSignal::new(
                            matched.to_params().into_iter().collect(),
                        );
                        #[allow(unused)]
                        let (view, child) = matched.into_view_and_child();
                        #[cfg(debug_assertions)]
                        if child.is_some() {
                            panic!(
                                "FlatRouter should not be used with a route \
                                 that has a child."
                            );
                        }

                        let  = RouteData {
                            params: ArcMemo::new({
                                let params = params.clone();
                                move |_| params.get()
                            }),
                            outlet: Default::default(),
                        };
                        let view = outer_owner.with(|| view.choose());
                        Either::Left(view)
                    }
                    _ => Either::Right((self.fallback)()),
                }
                .build()
            }
        })
    }

    fn rebuild(self, state: &mut Self::State) {}
}

impl<Rndr, Loc, FallbackFn, Fallback, Children> RenderHtml
    for FlatRouter<Rndr, Loc, Children, FallbackFn>
where
    Loc: Location + Send,
    FallbackFn: Fn() -> Fallback + Send + 'static,
    Fallback: RenderHtml,
    Children: MatchNestedRoutes + Send + 'static,
    Fallback::State: 'static,
    Rndr: Renderer + 'static,
{
    type AsyncOutput = Self;

    const MIN_LENGTH: usize =
        <Children::Match as MatchInterface>::View::MIN_LENGTH;

    async fn resolve(self) -> Self::AsyncOutput {
        self
    }

    fn to_html_with_buf(self, buf: &mut String, position: &mut Position, escape: bool, mark_branches: bool) {
        // if this is being run on the server for the first time, generating all possible routes
        if RouteList::is_generating() {
            // add routes
            let (base, routes) = self.routes.generate_routes();
            let mut routes = routes
                .into_iter()
                .map(|segments| {
                    let path = base
                        .into_iter()
                        .flat_map(|base| {
                            iter::once(PathSegment::Static(
                                base.to_string().into(),
                            ))
                        })
                        .chain(segments)
                        .collect::<Vec<_>>();
                    // TODO add non-defaults for mode, etc.
                    RouteListing::new(
                        path,
                        SsrMode::OutOfOrder,
                        [Method::Get],
                        None,
                    )
                })
                .collect::<Vec<_>>();

            // add fallback
            // TODO fix: causes overlapping route issues on Axum
            /*routes.push(RouteListing::new(
                [PathSegment::Static(
                    base.unwrap_or_default().to_string().into(),
                )],
                SsrMode::Async,
                [
                    Method::Get,
                    Method::Post,
                    Method::Put,
                    Method::Patch,
                    Method::Delete,
                ],
                None,
            ));*/

            RouteList::register(RouteList::from(routes));
        } else {
            let outer_owner = Owner::current()
                .expect("creating Router, but no Owner was found");
            let url = use_context::<RequestUrl>()
                .expect("could not find request URL in context");
            // TODO base
            let url =
                RequestUrl::parse(url.as_ref()).expect("could not parse URL");
            // TODO query params
            match self.routes.match_route(url.path()) {
                Some(new_match) => {
                    let params = ArcRwSignal::new(
                        new_match.to_params().into_iter().collect(),
                    );
                    #[allow(unused)]
                    let (view, child) = new_match.into_view_and_child();
                    #[cfg(debug_assertions)]
                    if child.is_some() {
                        panic!(
                            "FlatRouter should not be used with a route that \
                             has a child."
                        );
                    }

                    let  = RouteData {
                        params: ArcMemo::new({
                            let params = params.clone();
                            move |_| params.get()
                        }),
                        outlet: Default::default(),
                    };
                    let view = outer_owner.with(|| view.choose());
                    Either::Left(view)
                }
                None => Either::Right((self.fallback)()),
            }
            .to_html_with_buf(buf, position, escape)
        }
    }

    fn to_html_async_with_buf<const OUT_OF_ORDER: bool>(
        self,
        buf: &mut StreamBuilder, position: &mut Position, escape: bool, mark_branches: bool) where
        Self: Sized,
    {
        let outer_owner =
            Owner::current().expect("creating Router, but no Owner was found");
        let url = use_context::<RequestUrl>()
            .expect("could not find request URL in context");
        // TODO base
        let url = RequestUrl::parse(url.as_ref()).expect("could not parse URL");
        // TODO query params
        match self.routes.match_route(url.path()) {
            Some(new_match) => {
                let params = ArcRwSignal::new(
                    new_match.to_params().into_iter().collect(),
                );
                #[allow(unused)]
                let (view, child) = new_match.into_view_and_child();
                #[cfg(debug_assertions)]
                if child.is_some() {
                    panic!(
                        "FlatRouter should not be used with a route that has \
                         a child."
                    );
                }

                let  = RouteData {
                    params: ArcMemo::new({
                        let params = params.clone();
                        move |_| params.get()
                    }),
                    outlet: Default::default(),
                };
                let view = outer_owner.with(|| view.choose());
                Either::Left(view)
            }
            None => Either::Right((self.fallback)()),
        }
        .to_html_async_with_buf::<OUT_OF_ORDER>(buf, position, escape)
    }

    fn hydrate<const FROM_SERVER: bool>(
        self,
        cursor: &Cursor,
        position: &PositionState,
    ) -> Self::State {
        let location = Loc::new().unwrap(); // TODO
        location.init(self.base);
        let url = location.as_url().clone();
        let path = ArcMemo::new({
            let url = url.clone();
            move |_| url.read().path().to_string()
        });
        let search_params = ArcMemo::new({
            let url = url.clone();
            move |_| url.read().search_params().clone()
        });
        let outer_owner =
            Owner::current().expect("creating Router, but no Owner was found");
        let cursor = cursor.clone();
        let position = position.clone();

        RenderEffect::new(move |prev: Option<EitherState<_, _, _>>| {
            let path = path.read();
            let new_match = self.routes.match_route(&path);

            if let Some(mut prev) = prev {
                if let Some(new_match) = new_match {
                    let params = ArcRwSignal::new(
                        new_match.to_params().into_iter().collect(),
                    );
                    #[allow(unused)]
                    let (view, child) = new_match.into_view_and_child();
                    #[cfg(debug_assertions)]
                    if child.is_some() {
                        panic!(
                            "FlatRouter should not be used with a route that \
                             has a child."
                        );
                    }

                    let  = RouteData {
                        params: ArcMemo::new({
                            let params = params.clone();
                            move |_| params.get()
                        }),
                        outlet: Default::default(),
                    };
                    let view = outer_owner.with(|| view.choose());
                    Either::Left::<_, Fallback>(view).rebuild(&mut prev);
                } else {
                    Either::<<Children::Match as MatchInterface>::View, _>::Right((self.fallback)()).rebuild(&mut prev);
                }
                prev
            } else {
                match new_match {
                    Some(matched) => {
                        let params = ArcRwSignal::new(
                            matched.to_params().into_iter().collect(),
                        );
                        #[allow(unused)]
                        let (view, child) = matched.into_view_and_child();
                        #[cfg(debug_assertions)]
                        if child.is_some() {
                            panic!(
                                "FlatRouter should not be used with a route \
                                 that has a child."
                            );
                        }

                        let  = RouteData {
                            params: ArcMemo::new({
                                let params = params.clone();
                                move |_| params.get()
                            }),
                            outlet: Default::default(),
                        };
                        let view = outer_owner.with(|| view.choose());
                        Either::Left(view)
                    }
                    _ => Either::Right((self.fallback)()),
                }
                .hydrate::<FROM_SERVER>(&cursor, &position)
            }
        })
    }
}
