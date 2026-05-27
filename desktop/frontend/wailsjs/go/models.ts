export namespace apiclient {
	
	export class RemovedShow {
	    id: string;
	    name: string;
	    // Go type: time
	    date_added: any;
	    // Go type: time
	    last_played_at: any;
	
	    static createFrom(source: any = {}) {
	        return new RemovedShow(source);
	    }
	
	    constructor(source: any = {}) {
	        if ('string' === typeof source) source = JSON.parse(source);
	        this.id = source["id"];
	        this.name = source["name"];
	        this.date_added = this.convertValues(source["date_added"], null);
	        this.last_played_at = this.convertValues(source["last_played_at"], null);
	    }
	
		convertValues(a: any, classs: any, asMap: boolean = false): any {
		    if (!a) {
		        return a;
		    }
		    if (a.slice && a.map) {
		        return (a as any[]).map(elem => this.convertValues(elem, classs));
		    } else if ("object" === typeof a) {
		        if (asMap) {
		            for (const key of Object.keys(a)) {
		                a[key] = new classs(a[key]);
		            }
		            return a;
		        }
		        return new classs(a);
		    }
		    return a;
		}
	}
	export class AdvanceResult {
	    advanced_count: number;
	    removed_shows: RemovedShow[];
	
	    static createFrom(source: any = {}) {
	        return new AdvanceResult(source);
	    }
	
	    constructor(source: any = {}) {
	        if ('string' === typeof source) source = JSON.parse(source);
	        this.advanced_count = source["advanced_count"];
	        this.removed_shows = this.convertValues(source["removed_shows"], RemovedShow);
	    }
	
		convertValues(a: any, classs: any, asMap: boolean = false): any {
		    if (!a) {
		        return a;
		    }
		    if (a.slice && a.map) {
		        return (a as any[]).map(elem => this.convertValues(elem, classs));
		    } else if ("object" === typeof a) {
		        if (asMap) {
		            for (const key of Object.keys(a)) {
		                a[key] = new classs(a[key]);
		            }
		            return a;
		        }
		        return new classs(a);
		    }
		    return a;
		}
	}
	
	export class RoundEntry {
	    show_id: string;
	    show_name: string;
	    episode_id: string;
	    absolute_path: string;
	    order_value: number;
	
	    static createFrom(source: any = {}) {
	        return new RoundEntry(source);
	    }
	
	    constructor(source: any = {}) {
	        if ('string' === typeof source) source = JSON.parse(source);
	        this.show_id = source["show_id"];
	        this.show_name = source["show_name"];
	        this.episode_id = source["episode_id"];
	        this.absolute_path = source["absolute_path"];
	        this.order_value = source["order_value"];
	    }
	}

}

export namespace main {
	
	export class Status {
	    phase: string;
	    message: string;
	    round?: apiclient.RoundEntry[];
	    last_advance?: apiclient.AdvanceResult;
	
	    static createFrom(source: any = {}) {
	        return new Status(source);
	    }
	
	    constructor(source: any = {}) {
	        if ('string' === typeof source) source = JSON.parse(source);
	        this.phase = source["phase"];
	        this.message = source["message"];
	        this.round = this.convertValues(source["round"], apiclient.RoundEntry);
	        this.last_advance = this.convertValues(source["last_advance"], apiclient.AdvanceResult);
	    }
	
		convertValues(a: any, classs: any, asMap: boolean = false): any {
		    if (!a) {
		        return a;
		    }
		    if (a.slice && a.map) {
		        return (a as any[]).map(elem => this.convertValues(elem, classs));
		    } else if ("object" === typeof a) {
		        if (asMap) {
		            for (const key of Object.keys(a)) {
		                a[key] = new classs(a[key]);
		            }
		            return a;
		        }
		        return new classs(a);
		    }
		    return a;
		}
	}

}

